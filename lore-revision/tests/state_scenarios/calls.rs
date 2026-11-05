// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The catalogue's calls, as a recording run keeps them: each op's kind, task and interval in its
//! State's `seq`, holding the records it and the futures it spawned made but not those of its
//! windows, and what it answered or that it was dropped. Each explored run is replayed through the
//! model with its calls.

mod claims;
mod collected;
mod has;
mod links;
mod marks;
mod names;

use std::cell::Cell;
use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use lore_revision::node::BLOCK_NODE_COUNT;
use lore_revision::node::CHAIN_LOG_INITIAL;
use lore_revision::node::Node;
use lore_revision::node::NodeBlock;
use lore_revision::node::NodeExtFlags;
use lore_revision::node::NodeID;
use lore_revision::node::ROOT_NODE;
use lore_revision::state::ClearOutcome;
use lore_revision::state::Content;
use lore_revision::state::NameChange;
use lore_revision::state::node::hooks;
use lore_revision::state::node::hooks::log::Arm;
use lore_revision::state::node::hooks::log::Check;
use lore_revision::state::node::hooks::log::Effect;
use lore_revision::state::node::hooks::log::Expected;
use lore_revision::state::node::hooks::log::GuardEvent;
use lore_revision::state::node::hooks::log::Reason;
use lore_revision::state::node::hooks::log::Record;
use lore_revision::state::node::hooks::log::Section;
use lore_revision::state::node::hooks::log::Words;
use lore_storage::hash::hash_string;

use crate::harness::catalogue;
use crate::harness::catalogue::Kind;
use crate::harness::catalogue::call::Answer;
use crate::harness::catalogue::call::Call;
use crate::harness::catalogue::call::Filter;
use crate::harness::catalogue::call::Outcome;
use crate::harness::catalogue::call::Refusal;
use crate::harness::catalogue::call::Repeats;
use crate::harness::catalogue::call::SelfReported;
use crate::harness::catalogue::call::Spawn;
use crate::harness::catalogue::call::overlapping;
use crate::harness::catalogue::ops;
use crate::harness::catalogue::ops::directory;
use crate::harness::catalogue::ops::file;
use crate::harness::chaos;
use crate::harness::fixtures;
use crate::harness::fixtures::Tree;
use crate::harness::invariants::Rules;
use crate::harness::model::Start;
use crate::harness::scheduler;
use crate::harness::scheduler::Exploration;
use crate::harness::scheduler::Op;
use crate::harness::scheduler::Scenario;
use crate::harness::scheduler::Setup;

/// An op of a function, so ops of one shape fit in an array.
type OpFn = fn(Arc<Tree>) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>>;

/// What a check reads of a run.
struct Run<'a> {
    tree: &'a Tree,
    records: Vec<Record>,
    calls: Vec<Call>,
    ops: Vec<usize>,
    spawns: Vec<Spawn>,
}

impl Run<'_> {
    /// The records the tree's State took by `op`'s tasks.
    fn of_op(&self, op: usize) -> Vec<&Record> {
        let state = self.tree.state.hooks().id();
        self.records
            .iter()
            .filter(|record| {
                record.state == state
                    && record.task.and_then(|task| self.ops.get(task)) == Some(&op)
            })
            .collect()
    }
}

/// Every run of `ops` on the tree `setup` builds, recording, each handed to `check`.
fn explore(
    setup: Setup<Tree>,
    ops: Vec<Op<Tree>>,
    cancellable: Vec<usize>,
    check: impl Fn(&Run<'_>) + 'static,
) {
    explore_with(Explore::default(), setup, ops, cancellable, check);
}

/// How [`explore_with`] explores: whether readers park at their own points, the operations with
/// steps outside every call, whose records no call need hold, the self-reports it admits, whether
/// it drops calls of kinds their production callers never drop, the preemptions a run may make,
/// where not every schedule is run, and the invariants each run keeps at its end, where they are
/// checked.
#[derive(Clone, Copy, Default)]
struct Explore {
    reader_points: bool,
    outside: &'static [usize],
    self_reports: &'static [SelfReported],
    uncancellable_drops: bool,
    preemptions: Option<usize>,
    rules: Option<Rules>,
}

/// [`explore`] as `how` says. Each record of an operation not `outside` is held by one call, or by
/// none where its caller made it in a call's window ([`one_call_each`]), and the run replays
/// through the model started where its setup ended, with its calls.
fn explore_with(
    how: Explore,
    setup: Setup<Tree>,
    ops: Vec<Op<Tree>>,
    cancellable: Vec<usize>,
    check: impl Fn(&Run<'_>) + 'static,
) {
    let check = Rc::new(check);
    // The run's start, read where its setup ends; the setup and the check run one after the other
    // on the exploring thread.
    let start: Rc<Cell<Start>> = Rc::default();
    let read = start.clone();
    let setup: Setup<Tree> = Box::new(move || {
        let (read, made) = (read.clone(), setup());
        Box::pin(async move {
            let tree = made.await;
            read.set(Start::read(&tree).await);
            tree
        })
    });
    let through_calls: Vec<usize> = (0..ops.len())
        .filter(|op| !how.outside.contains(op))
        .collect();
    let scenario = Scenario::<Tree> {
        name: "calls",
        setup,
        ops,
        cancellable: cancellable.clone(),
        check: Box::new(move |tree: Arc<Tree>| {
            let (check, through_calls, start) =
                (check.clone(), through_calls.clone(), start.take());
            Box::pin(async move {
                let run = Run {
                    tree: &tree,
                    records: scheduler::run_log(),
                    calls: scheduler::run_calls(),
                    ops: scheduler::run_ops(),
                    spawns: scheduler::run_spawns(),
                };
                for op in through_calls {
                    one_call_each(&run, op);
                }
                let mut model = start.model(&*tree, &run.records).await;
                model.admitting_self_reports(how.self_reports);
                if how.uncancellable_drops {
                    model.admitting_uncancellable_drops();
                }
                model.gated();
                model.replay_called(&run.records, run.calls.clone(), run.spawns.clone());
                let found: Vec<String> =
                    model.violations().iter().map(ToString::to_string).collect();
                assert!(found.is_empty(), "the model found: {found:#?}");
                if let Some(rules) = how.rules {
                    crate::holding(&tree.state, &tree.repository, rules).await;
                }
                check(&run);
            })
        }),
    };
    let exploration = Exploration {
        recording: true,
        cancellation: !cancellable.is_empty(),
        reader_points: how.reader_points,
        ..Exploration::default()
    };
    // Sleep sets prune soundly only an exploration that runs every schedule.
    let exploration = match how.preemptions {
        Some(preemptions) => Exploration {
            preemptions,
            deepening: false,
            sleep_sets: false,
            ..exploration
        },
        None => exploration,
    };
    let covered = scenario.explore(exploration);
    // Every schedule, or every one within the bound where the bound limited it.
    assert!(
        covered.complete
            || how
                .preemptions
                .is_some_and(|bound| covered.bound == Some(bound)),
        "explored {covered:?}"
    );
}

/// Asserts each record `op` made on the tree's State is one call's, or, made by its caller in a
/// window of one of its task's calls, none's.
fn one_call_each(run: &Run<'_>, op: usize) {
    for record in run.of_op(op) {
        let holding = run
            .calls
            .iter()
            .filter(|call| call.holds(record, &run.spawns))
            .count();
        let windowed = run.calls.iter().any(|call| {
            call.task == record.task
                && call
                    .windows
                    .iter()
                    .any(|&(begin, end)| (begin..end).contains(&record.seq))
        });
        assert!(
            holding == 1 || (holding == 0 && windowed),
            "{record:?} is {holding} calls': {:#?}",
            run.calls
        );
    }
}

fn tree(paths: &'static [&'static str]) -> Setup<Tree> {
    Box::new(move || Box::pin(Tree::new(paths)))
}

fn served(paths: &'static [&'static str]) -> Setup<Tree> {
    Box::new(move || {
        Box::pin(async move {
            let tree = Tree::new(paths).await;
            tree.state.mark_served();
            tree
        })
    })
}

/// A change a setup makes to its tree once built, unrecorded.
type Then = fn(&Tree) -> std::pin::Pin<Box<dyn Future<Output = ()> + '_>>;

/// A tree of `paths` after `then` changed it, unrecorded, as a setup does.
fn changed(paths: &'static [&'static str], then: Then) -> Setup<Tree> {
    Box::new(move || {
        Box::pin(async move {
            let tree = Tree::new(paths).await;
            then(&tree).await;
            tree
        })
    })
}

/// A dirty move renaming the node at `path`, a file or a directory, to `name` within its
/// directory, in place, as `lore dirty move` renames one: the one in-place rename a caller makes
/// that changes the name's hash, run beside readers alone.
async fn renamed_within(tree: &Tree, path: &str, name: &str) {
    let (parent, _) = path.rsplit_once('/').expect("a path below a directory");
    ops::dirty_move(
        tree,
        tree.id(path),
        tree.id(parent),
        NameChange::Rename(name),
    )
    .await
    .unwrap_or_else(|error| panic!("'{path}' is renamed: {error}"));
}

/// A dirty move of the node at `path` into the directory at `to`, its name kept, as
/// `lore dirty move` makes one.
async fn dirty_moved(tree: &Tree, path: &str, to: &str) {
    let (node, into) = (tree.id(path), tree.id(to));
    (ops::dirty_move(tree, node, into, NameChange::Keep).await)
        .unwrap_or_else(|error| panic!("'{path}' moves: {error}"));
}

/// A stage's move of the node at `path` into the directory at `to`, its name kept and free there.
async fn stage_moved(tree: &Tree, path: &str, to: &str) {
    let (node, into) = (tree.id(path), tree.id(to));
    (ops::stage_move(tree, node, into, NameChange::Keep, None).await)
        .unwrap_or_else(|error| panic!("'{path}' moves: {error}"));
}

/// A realized move of the node at `path` into the directory at `to`, its name kept, outside a
/// merge.
async fn realize_moved(tree: &Tree, path: &str, to: &str) {
    let (node, into) = (tree.id(path), tree.id(to));
    (ops::realize_move(tree, node, into, NameChange::Keep, false).await)
        .unwrap_or_else(|error| panic!("'{path}' moves: {error}"));
}

/// The trusted moves of the node at `$path` into `e`, each with its kind: `lore dirty move`'s, the
/// stage's, and the realize's outside a merge.
macro_rules! moves_into_e {
    ($path:literal) => {
        [
            (
                |tree| Box::pin(async move { dirty_moved(&tree, $path, "e").await }),
                &catalogue::DIRTY_MOVE,
            ),
            (
                |tree| Box::pin(async move { stage_moved(&tree, $path, "e").await }),
                &catalogue::STAGE_MOVE,
            ),
            (
                |tree| Box::pin(async move { realize_moved(&tree, $path, "e").await }),
                &catalogue::REALIZE_MOVE,
            ),
        ]
    };
}

/// The trusted moves of `d` into `e`, each with its kind.
fn moves_of_d() -> [(OpFn, &'static Kind); 3] {
    moves_into_e!("d")
}

/// The mark a trusted move of a directory leaves on each node below it: the dirty move's and the
/// stage's; none for the realize move, which marks no subtree.
fn move_mark(by: &Kind) -> Option<fn(&Node) -> bool> {
    if std::ptr::eq(by, &catalogue::DIRTY_MOVE) {
        Some(Node::is_dirty_move as fn(&Node) -> bool)
    } else if std::ptr::eq(by, &catalogue::STAGE_MOVE) {
        Some(Node::is_staged_move as fn(&Node) -> bool)
    } else {
        None
    }
}

/// A tree of `paths` whose `p/x` an unserved detach took, unrecorded.
fn x_gone(paths: &'static [&'static str]) -> Setup<Tree> {
    changed(paths, |t| {
        Box::pin(async move { assert!(ops::unserved_detach(t, t.id("p/x")).await) })
    })
}

/// A tree whose directory `s/t` a C API add staged, for a delete to queue pending.
fn staged_directory() -> Setup<Tree> {
    Box::new(|| {
        Box::pin(async {
            let mut tree = Tree::new(&["s/", "a/"]).await;
            let t = ops::staged_add(&tree, tree.id("s"), directory("t"), "t")
                .await
                .expect("'s/t' adds");
            tree.nodes.push(("s/t".to_string(), t));
            tree
        })
    })
}

#[test]
fn a_call_records_its_kind_task_interval_and_outcome() {
    explore(
        tree(&["d/f"]),
        vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let created = ops::checked_create(&tree, tree.id("d"), "n").await;
                assert!(created.is_some(), "'d/n' is created");
            })
        })],
        vec![],
        |run| {
            let [call] = run.calls.as_slice() else {
                panic!("one call: {:#?}", run.calls);
            };
            assert!(std::ptr::eq(call.kind, &catalogue::CHECKED_CREATE));
            assert_eq!(call.task, Some(0));
            assert_eq!(call.state, run.tree.state.hooks().id());
            let published = run.records.iter().find_map(|record| match record.effect {
                Effect::Publish { node, .. } => Some(node),
                _ => None,
            });
            assert_eq!(
                Some(&call.outcome),
                published
                    .map(|node| Outcome::Done(Answer::Node(node)))
                    .as_ref(),
                "the create answers the node it published"
            );
            let made = run.of_op(0);
            assert!(!made.is_empty(), "the create recorded");
            for record in made {
                assert!(
                    call.holds(record, &run.spawns),
                    "{record:?} outside [{}, {})",
                    call.begin,
                    call.end
                );
            }
        },
    );
}

/// A call dropped part way ends as it drops, holding every record its operation made, and
/// one that ran is done: the exploration reaches both.
#[test]
fn a_dropped_call_holds_what_its_operation_made() {
    let (dropped, done) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
    let (counts_dropped, counts_done) = (dropped.clone(), done.clone());
    let dropped_armed = Rc::new(Cell::new(0));
    let counts_dropped_armed = dropped_armed.clone();
    explore(
        tree(&["a/x", "b/"]),
        vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                ops::staged_move(&tree, tree.id("a/x"), tree.id("b"), "x").await;
            })
        })],
        vec![0],
        move |run| {
            let [call] = run.calls.as_slice() else {
                panic!("one call: {:#?}", run.calls);
            };
            let made = run.of_op(0);
            assert!(
                made.iter().all(|record| call.holds(record, &run.spawns)),
                "{call:?} holds its operation's records"
            );
            assert!(made.is_empty() || call.begin < call.end);
            match call.outcome {
                Outcome::Dropped => counts_dropped.set(counts_dropped.get() + 1),
                Outcome::Done(Answer::None) => counts_done.set(counts_done.get() + 1),
                ref other => panic!("the move answered {other:?}"),
            }
            let guard_dropped = made.iter().any(|record| {
                record.effect
                    == Effect::Guard {
                        event: GuardEvent::DroppedArmed,
                    }
            });
            if guard_dropped {
                assert_eq!(
                    call.outcome,
                    Outcome::Dropped,
                    "only a dropped move drops its guard"
                );
                counts_dropped_armed.set(counts_dropped_armed.get() + 1);
            }
        },
    );
    assert!(
        dropped.get() > 0 && done.get() > 0,
        "dropped {} and done {}",
        dropped.get(),
        done.get()
    );
    assert!(
        dropped_armed.get() > 0,
        "some move dropped between its climb and its move drops its guard armed"
    );
}

/// Calls one task makes one after the other have intervals one after the other, each holding its
/// own records.
#[test]
fn the_calls_of_one_task_follow_one_another() {
    explore(
        tree(&["d/f"]),
        vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                ops::scan_mark_modified(&tree, tree.id("d/f")).await;
                ops::scan_clear(&tree, tree.id("d/f")).await;
            })
        })],
        vec![],
        |run| {
            let [mark, clear] = run.calls.as_slice() else {
                panic!("two calls: {:#?}", run.calls);
            };
            assert!(std::ptr::eq(mark.kind, &catalogue::SCAN_MARK_MODIFIED));
            assert!(std::ptr::eq(clear.kind, &catalogue::SCAN_CLEAR));
            assert!(mark.end <= clear.begin, "{mark:?} then {clear:?}");
            let made = run.of_op(0);
            for record in &made {
                assert!(
                    mark.holds(record, &run.spawns) != clear.holds(record, &run.spawns),
                    "{record:?} held by one call"
                );
            }
            for call in [mark, clear] {
                assert!(made.iter().any(|record| call.holds(record, &run.spawns)));
            }
        },
    );
}

/// A call made in a future its operation spawned is that future's task's, which names the
/// operation, and holds that future's records.
#[test]
fn a_call_in_a_spawned_future_names_its_operation() {
    explore(
        tree(&["d/f"]),
        vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let marked = lore_base::lore_spawn!(hooks::inherit(async move {
                    ops::scan_mark_modified(&tree, tree.id("d/f")).await;
                }));
                marked.await.expect("the spawned mark ends");
            })
        })],
        vec![],
        |run| {
            let [call] = run.calls.as_slice() else {
                panic!("one call: {:#?}", run.calls);
            };
            let task = call.task.expect("the call's task");
            assert!(task > 0, "the spawned future's own task");
            assert_eq!(run.ops[task], 0, "the spawned future is the operation's");
            assert_eq!(
                run.spawns[task].spawner,
                Some(0),
                "spawned by the operation"
            );
            let made = run.of_op(0);
            assert!(!made.is_empty());
            assert!(made.iter().all(|record| call.holds(record, &run.spawns)));
        },
    );
}

/// A call holds its task's records within its interval and outside its windows, and those of the
/// futures spawned in its own steps, through every spawn, past its end too; not those of its
/// spawner, of another operation, of a future its task spawned in another call or outside every
/// call, or of another State.
#[test]
fn a_call_holds_what_its_task_spawned_and_no_more() {
    let record = |task: usize, seq: u64| Record {
        seq,
        sub: 0,
        section: Section::Unlabelled,
        state: 1,
        task: Some(task),
        effect: Effect::Served,
    };
    let call = |id: u64, task: usize| Call {
        id,
        ..made_up(task, (3, 8), &[(6, 7)])
    };
    let spawn = |spawner: usize, call: Option<u64>| Spawn {
        spawner: Some(spawner),
        call,
    };
    // Tasks 0 and 1 are operations; 2 spawned by 0 in call 10, 3 by 2 in call 12, 4 by 1 in call
    // 11, 5 by 0 in call 13, 6 by 0 outside every call.
    let spawns = [
        Spawn::default(),
        Spawn::default(),
        spawn(0, Some(10)),
        spawn(2, Some(12)),
        spawn(1, Some(11)),
        spawn(0, Some(13)),
        spawn(0, None),
    ];
    let (zero, two) = (call(10, 0), call(12, 2));
    assert!(zero.holds(&record(0, 5), &spawns), "its own record");
    assert!(zero.holds(&record(3, 5), &spawns), "a grandchild's record");
    assert!(two.holds(&record(3, 5), &spawns), "a child's record");
    assert!(
        zero.holds(&record(2, 9), &spawns),
        "a child's record after its end"
    );
    assert!(!two.holds(&record(0, 5), &spawns), "its spawner's record");
    assert!(!zero.holds(&record(4, 5), &spawns), "another operation's");
    assert!(
        !zero.holds(&record(5, 5), &spawns),
        "spawned in another call"
    );
    assert!(
        !zero.holds(&record(6, 5), &spawns),
        "spawned outside every call"
    );
    assert!(!zero.holds(&record(0, 6), &spawns), "a window's record");
    assert!(!zero.holds(&record(0, 8), &spawns), "past its end");
    assert!(!zero.holds(&record(0, 2), &spawns), "before its begin");
    assert!(
        !zero.holds(&record(2, 2), &spawns),
        "a child's before its begin"
    );
    let elsewhere = Record {
        state: 2,
        ..record(0, 5)
    };
    assert!(!zero.holds(&elsewhere, &spawns), "another State's record");
}

/// A made-up call of task `task` on State 1, its interval and windows as given.
fn made_up(task: usize, (begin, end): (u64, u64), windows: &[(u64, u64)]) -> Call {
    Call {
        kind: &catalogue::SCAN_CLEAR,
        sweeping: None,
        id: 0,
        task: Some(task),
        state: 1,
        target: None,
        destination: None,
        name: None,
        spelling: None,
        holder: None,
        place: None,
        listing: None,
        prepends: 0,
        taken: Vec::new(),
        leaves: None,
        lookups: Vec::new(),
        walks: Vec::new(),
        paths: Vec::new(),
        names: Vec::new(),
        failed_reads: 0,
        begin,
        end,
        windows: windows.to_vec(),
        outcome: Outcome::Done(Answer::None),
    }
}

/// Calls of one task nest only by their windows, however deep and in whichever order equal
/// intervals come; any other overlap is found.
#[test]
fn one_tasks_calls_nest_only_by_their_windows() {
    let legal = [
        // One after another, the second beginning where the first ended.
        vec![made_up(0, (0, 4), &[]), made_up(0, (4, 6), &[])],
        // Nested two deep, by windows.
        vec![
            made_up(0, (0, 20), &[(2, 18)]),
            made_up(0, (3, 17), &[(5, 15)]),
            made_up(0, (6, 14), &[]),
        ],
        // Two calls one after another in one window.
        vec![
            made_up(0, (0, 20), &[(2, 12)]),
            made_up(0, (3, 6), &[]),
            made_up(0, (7, 11), &[]),
        ],
        // Equal intervals, the window's call first or second.
        vec![made_up(0, (2, 9), &[]), made_up(0, (2, 9), &[(2, 9)])],
        vec![made_up(0, (2, 9), &[(2, 9)]), made_up(0, (2, 9), &[])],
        // Another task's, or another State's, overlapping.
        vec![made_up(0, (0, 10), &[]), made_up(1, (2, 5), &[])],
        vec![
            made_up(0, (0, 10), &[]),
            Call {
                state: 2,
                ..made_up(0, (2, 5), &[])
            },
        ],
        // A call that took no seq, inside another.
        vec![made_up(0, (0, 10), &[]), made_up(0, (4, 4), &[])],
    ];
    for calls in &legal {
        assert!(overlapping(calls).is_none(), "{calls:#?}");
    }
    let faults = [
        // Overlapping, no window.
        vec![made_up(0, (0, 10), &[]), made_up(0, (5, 15), &[])],
        // Outside the window, after a windowed call ended.
        vec![
            made_up(0, (0, 20), &[(2, 8)]),
            made_up(0, (3, 6), &[]),
            made_up(0, (9, 12), &[]),
        ],
        // Inside the outer's window, but overlapping a sibling there that has no window.
        vec![
            made_up(0, (0, 20), &[(2, 18)]),
            made_up(0, (3, 12), &[]),
            made_up(0, (5, 8), &[]),
        ],
        // Running past the window.
        vec![made_up(0, (0, 20), &[(2, 8)]), made_up(0, (3, 9), &[])],
        // Equal intervals, neither in the other's window.
        vec![made_up(0, (2, 9), &[]), made_up(0, (2, 9), &[])],
    ];
    for calls in &faults {
        assert!(overlapping(calls).is_some(), "{calls:#?}");
    }
}

/// A call made in another's own steps, outside its windows, is a composition the catalogue does
/// not declare, and panics, whether or not a run records.
#[tokio::test]
#[should_panic(expected = "inside another call")]
async fn a_call_inside_another_panics() {
    crate::in_context(async {
        let tree = Tree::new(&["d/f"]).await;
        catalogue::call::call(&tree.state, (&catalogue::SCAN_CLEAR, None), async {
            ops::scan_mark_modified(&tree, tree.id("d/f")).await;
            ((), Outcome::Done(Answer::None))
        })
        .await;
    })
    .await;
}

/// A call made in a future spawned in another call's own steps holds records the outer call holds
/// too: the run fails on it.
#[test]
#[should_panic(expected = "held by two calls")]
fn a_call_overlapping_one_its_task_spawned_fails_the_run() {
    explore(
        tree(&["d/f"]),
        vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let spawning = tree.clone();
                catalogue::call::call(&tree.state, (&catalogue::SCAN_CLEAR, None), async move {
                    let marked = lore_base::lore_spawn!(hooks::inherit(async move {
                        ops::scan_mark_modified(&spawning, spawning.id("d/f")).await;
                    }));
                    marked.await.expect("the spawned mark ends");
                    ((), Outcome::Done(Answer::None))
                })
                .await;
            })
        })],
        vec![],
        |_| {},
    );
}

/// A call that took no `seq`, then another of its task's at the same `seq`, follow one another:
/// the first holds no record, so they are no overlap.
#[test]
fn a_call_that_took_no_seq_overlaps_none() {
    explore(
        tree(&["d/e/"]),
        vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                assert!(ops::find_dir(&tree, "d/e").await.is_some());
                ops::scan_mark_modified(&tree, tree.id("d/e")).await;
            })
        })],
        vec![],
        |run| {
            let [find, mark] = run.calls.as_slice() else {
                panic!("two calls: {:#?}", run.calls);
            };
            assert_eq!(find.begin, find.end, "the find took no seq: {find:?}");
            assert_eq!(find.end, mark.begin, "{find:?} then {mark:?}");
        },
    );
}

/// Two operations' calls on one State hold only their own operation's records, in runs where
/// they overlap.
#[test]
fn overlapping_calls_hold_only_their_own_records() {
    let overlapped = Rc::new(Cell::new(0));
    let counts = overlapped.clone();
    explore(
        tree(&["a/x", "b/y"]),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    ops::scan_mark_modified(&tree, tree.id("a/x")).await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    ops::scan_mark_modified(&tree, tree.id("b/y")).await;
                })
            }),
        ],
        vec![],
        move |run| {
            let [first, second] = run.calls.as_slice() else {
                panic!("two calls: {:#?}", run.calls);
            };
            if first.begin < second.end && second.begin < first.end {
                counts.set(counts.get() + 1);
            }
            for call in [first, second] {
                let op = run.ops[call.task.expect("the call's task")];
                let made = run.of_op(op);
                assert!(made.iter().all(|record| call.holds(record, &run.spawns)));
                let other = run.of_op(1 - op);
                assert!(!other.iter().any(|record| call.holds(record, &run.spawns)));
            }
        },
    );
    assert!(overlapped.get() > 0, "no run overlapped the calls");
}

/// A call made in another's window is a call of its own, whose records the outer call does not
/// hold.
#[test]
fn a_call_in_a_window_is_its_own() {
    explore(
        staged_directory(),
        vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let place = (tree.id("s/t"), tree.id("s"), hash_string("t"));
                let added = async {
                    let added = ops::staged_add(&tree, tree.id("a"), file("n"), "n").await;
                    assert!(added.is_some(), "'a/n' adds");
                };
                assert!(ops::pend(&tree, place, added).await, "'s/t' is detached");
            })
        })],
        vec![],
        |run| {
            let [pend, add] = run.calls.as_slice() else {
                panic!("two calls: {:#?}", run.calls);
            };
            assert!(std::ptr::eq(pend.kind, &catalogue::PEND));
            assert!(std::ptr::eq(add.kind, &catalogue::STAGED_ADD));
            assert_eq!(pend.windows.len(), 1, "{pend:?}");
            let made = run.of_op(0);
            assert!(
                made.iter().any(|record| add.holds(record, &run.spawns)),
                "the add recorded"
            );
            for record in &made {
                assert!(
                    pend.holds(record, &run.spawns) != add.holds(record, &run.spawns),
                    "{record:?} is one call's"
                );
            }
        },
    );
}

/// A call dropped in its window closes the window as it drops, then itself after its own steps'
/// drop glue: each record is one call's, the moves permit's release the pend's. The pend stands for
/// any call with a window here, though its caller never drops it, as it runs in a task the C API
/// call awaits.
#[test]
fn a_call_dropped_in_its_window_holds_its_drop_glue() {
    let reached = Rc::new(Cell::new(0));
    let counts = reached.clone();
    explore_with(
        Explore {
            uncancellable_drops: true,
            ..Explore::default()
        },
        staged_directory(),
        vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let place = (tree.id("s/t"), tree.id("s"), hash_string("t"));
                let added = async {
                    let _ = ops::staged_add(&tree, tree.id("a"), file("n"), "n").await;
                };
                ops::pend(&tree, place, added).await;
            })
        })],
        vec![0],
        move |run| {
            let (pend, add) = match run.calls.as_slice() {
                [pend] => (pend, None),
                [pend, add] => (pend, Some(add)),
                calls => panic!("one or two calls: {calls:#?}"),
            };
            assert!(std::ptr::eq(pend.kind, &catalogue::PEND));
            for &(begin, end) in &pend.windows {
                assert!(
                    pend.begin <= begin && begin <= end && end <= pend.end,
                    "{pend:?}"
                );
            }
            let Some(add) = add else {
                return;
            };
            if pend.outcome == Outcome::Dropped && add.outcome == Outcome::Dropped {
                counts.set(counts.get() + 1);
            }
            for record in &run.of_op(0) {
                let holding = [pend, add]
                    .iter()
                    .filter(|call| call.holds(record, &run.spawns))
                    .count();
                assert_eq!(holding, 1, "{record:?} is one call's: {pend:?} {add:?}");
                if matches!(record.effect, Effect::Unpermit) {
                    assert!(pend.holds(record, &run.spawns), "the pend's {record:?}");
                }
            }
        },
    );
    assert!(reached.get() > 0, "no run dropped the pend in its window");
}

/// A tree holding `g`, with the tracked `h`, holding `i`, and `j`, and the added `n`, holding
/// `f`, and `m`, as the C API adds them.
fn batch_tree() -> Setup<Tree> {
    Box::new(|| {
        Box::pin(async {
            let mut tree = Tree::new(&["g/h/i", "g/j"]).await;
            let (state, repository) = (tree.state.clone(), tree.repository.clone());
            let added = |parent: NodeID, node: Node, name: &'static str| {
                let (state, repository) = (state.clone(), repository.clone());
                async move {
                    state
                        .node_add_staged(repository, parent, node, name)
                        .await
                        .unwrap_or_else(|error| panic!("'{name}' adds: {error}"))
                }
            };
            let n = added(tree.id("g"), directory("n"), "n").await;
            let f = added(n, file("f"), "f").await;
            let m = added(tree.id("g"), file("m"), "m").await;
            tree.nodes.extend([
                ("g/n".to_string(), n),
                ("g/n/f".to_string(), f),
                ("g/m".to_string(), m),
            ]);
            tree
        })
    })
}

/// A C API delete's batch of the tracked `g/h` and `g/j` and the added `g/n` and `g/m`, beside a
/// C API add into `g/h`: each level and each detach a call of its own, spread over futures the
/// batch spawns, every record one call's, each inside the batch's moves permit by its batch's rule
/// (C10), each level climbing first (H8), every run replaying through the model clean. Every
/// entry goes whole; the add lands before `g/h`'s level, which reaches it as an added root and
/// detaches it, or is refused after. Within four preemptions a run.
#[test]
fn a_delete_batch_steps_in_calls_of_their_own_inside_its_permit() {
    // The runs whose add landed, and those whose add was refused.
    let ended = Rc::new(Cell::new((0, 0)));
    let counts = ended.clone();
    explore_with(
        Explore {
            preemptions: Some(4),
            ..Explore::default()
        },
        batch_tree(),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let roots = ["g/h", "g/j", "g/n", "g/m"].map(|path| tree.id(path));
                    let went = ops::delete_batch(&tree, &roots).await;
                    assert_eq!(went, [Some(true); 4], "every entry goes whole");
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let _ = ops::staged_add(&tree, tree.id("g/h"), file("x"), "x").await;
                })
            }),
        ],
        vec![],
        move |run| {
            let of_kind = |kind: &'static Kind| {
                (run.calls.iter())
                    .filter(|call| std::ptr::eq(call.kind, kind))
                    .collect::<Vec<&Call>>()
            };
            let (batch, levels) = (
                of_kind(&catalogue::DELETE_BATCH),
                of_kind(&catalogue::DELETE_LEVEL),
            );
            let detaches = of_kind(&catalogue::DETACH_ADDED);
            let [batch] = batch.as_slice() else {
                panic!("one batch: {:#?}", run.calls);
            };
            let adds = of_kind(&catalogue::STAGED_ADD);
            let [add] = adds.as_slice() else {
                panic!("one add: {:#?}", run.calls);
            };
            let landed = matches!(add.outcome, Outcome::Done(_));
            assert_eq!(
                levels.len(),
                3,
                "'g/h', 'g/j' and 'g/h/i' staged: {levels:#?}"
            );
            assert_eq!(
                detaches.len(),
                2 + usize::from(landed),
                "'g/n', 'g/m' and a landed add detached: {detaches:#?}"
            );
            // The first level's two nodes and the detaches from two directories, where the add
            // landed, ran in futures the batch's task spawned.
            let spawned = |call: &&Call| {
                call.task
                    .and_then(|task| run.spawns.get(task))
                    .is_some_and(|spawn| spawn.spawner == batch.task)
            };
            assert!(
                levels.iter().copied().filter(spawned).count() >= 2,
                "the first level spread over futures: {levels:#?}"
            );
            for call in levels.iter().chain(&detaches) {
                assert!(
                    batch.begin < call.begin && call.end < batch.end,
                    "{call:?} within the batch {batch:?}"
                );
            }
            let (landed_runs, refused_runs) = counts.get();
            counts.set(if landed {
                (landed_runs + 1, refused_runs)
            } else {
                (landed_runs, refused_runs + 1)
            });
        },
    );
    let (landed, refused) = ended.get();
    assert!(
        landed > 0 && refused > 0,
        "the add landed in {landed} runs, was refused in {refused}"
    );
}

/// A future spawned in a call's own steps is the call's to its last record, after the call drops
/// too, as a `JoinSet`'s futures run on to the runtime's next turn.
#[test]
fn a_dropped_call_holds_what_its_spawned_futures_record_after_it() {
    let stragglers = Rc::new(Cell::new(0));
    let counts = stragglers.clone();
    explore(
        tree(&["d/f", "e/g"]),
        vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let spawning = tree.clone();
                catalogue::call::call(&tree.state, (&catalogue::SCAN_CLEAR, None), async {
                    let marked = lore_base::lore_spawn!(hooks::inherit(async move {
                        ops::mark_dirty(&spawning, spawning.id("d/f")).await;
                    }));
                    ops::mark_dirty(&tree, tree.id("e/g")).await;
                    marked.await.expect("the spawned mark ends");
                    ((), Outcome::Done(Answer::None))
                })
                .await;
            })
        })],
        vec![0],
        move |run| {
            let [call] = run.calls.as_slice() else {
                panic!("one call: {:#?}", run.calls);
            };
            let made = run.of_op(0);
            assert!(
                made.iter().any(|record| record.task != Some(0)),
                "the spawned mark recorded"
            );
            for record in &made {
                assert!(call.holds(record, &run.spawns), "{record:?} is {call:?}'s");
                if record.task != Some(0) && record.seq >= call.end {
                    counts.set(counts.get() + 1);
                }
            }
        },
    );
    assert!(
        stragglers.get() > 0,
        "no spawned future recorded after its call dropped"
    );
}

/// A settle dropped in its window drops its pending clear after the window closes, inside the
/// settle: the clear's drop is the settle's, and each record one call's.
#[test]
fn a_settle_dropped_in_its_window_holds_its_pending_clears_drop() {
    let reached = Rc::new(Cell::new(0));
    let counts = reached.clone();
    explore(
        marked(&["d/f"]),
        vec![Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let created = async {
                    let _ = ops::served_create(&t, t.id("d"), "n").await;
                };
                ops::settle_waiting(&t, t.id("d"), created).await;
            })
        })],
        vec![0],
        move |run| {
            let settle = &run.calls[0];
            assert!(std::ptr::eq(settle.kind, &catalogue::SETTLE));
            let Some(create) = run.calls.get(1) else {
                return;
            };
            if settle.outcome == Outcome::Dropped && create.outcome == Outcome::Dropped {
                counts.set(counts.get() + 1);
            }
            for record in run.of_op(0) {
                let dropped_clear = record.section == Section::SettlePhase3
                    && matches!(
                        record.effect,
                        Effect::Word {
                            clear_pending: false,
                            ..
                        }
                    );
                if dropped_clear {
                    assert!(settle.holds(record, &run.spawns), "the settle's {record:?}");
                }
            }
        },
    );
    assert!(reached.get() > 0, "no run dropped the settle in its window");
}

/// A swap's paced copy dropped in its pace drops its build after the pace's window closes, inside
/// the copy: each record one call's.
#[test]
fn a_paced_copy_dropped_in_its_pace_holds_its_builds_drop() {
    let reached = Rc::new(Cell::new(0));
    let counts = reached.clone();
    explore(
        served(&["d/", "e/"]),
        vec![Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let marking = t.clone();
                let pace = move || {
                    let marking = marking.clone();
                    async move {
                        let _ = ops::served_mark_dir(&marking, marking.id("e")).await;
                    }
                };
                ops::swap_copy_paced(&t, (t.id("d"), "c"), &["i", "j"], pace).await;
            })
        })],
        vec![0],
        move |run| {
            let copy = &run.calls[0];
            assert!(std::ptr::eq(copy.kind, &catalogue::SWAP_COPY_TREE));
            if copy.outcome == Outcome::Dropped
                && run.calls[1..]
                    .iter()
                    .any(|mark| mark.outcome == Outcome::Dropped)
            {
                counts.set(counts.get() + 1);
            }
        },
    );
    assert!(reached.get() > 0, "no run dropped the copy in its pace");
}

/// A future spawned in a task's first call that records once its second call has begun is the
/// first call's alone: each call's id is its own.
#[test]
fn a_straggler_of_a_tasks_first_call_is_not_its_seconds() {
    let late = Rc::new(Cell::new(0));
    let counts = late.clone();
    explore(
        tree(&["d/f", "e/g"]),
        vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let spawning = tree.clone();
                let marked =
                    catalogue::call::call(&tree.state, (&catalogue::SCAN_CLEAR, None), async {
                        let marked = lore_base::lore_spawn!(hooks::inherit(async move {
                            ops::mark_dirty(&spawning, spawning.id("d/f")).await;
                        }));
                        (marked, Outcome::Done(Answer::None))
                    })
                    .await;
                ops::scan_mark_modified(&tree, tree.id("e/g")).await;
                marked.await.expect("the spawned mark ends");
            })
        })],
        vec![],
        move |run| {
            let [first, second] = run.calls.as_slice() else {
                panic!("two calls: {:#?}", run.calls);
            };
            for record in run.of_op(0) {
                if record.task != Some(0) {
                    assert!(
                        first.holds(record, &run.spawns),
                        "{record:?} is the first's"
                    );
                    if record.seq >= second.begin {
                        counts.set(counts.get() + 1);
                    }
                }
            }
        },
    );
    assert!(
        late.get() > 0,
        "no straggler recorded once the second call began"
    );
}

/// A future spawned in a call's window is its caller's, not the call's, and a call it makes is one
/// of its own, which the run takes for no fault.
#[test]
fn a_future_spawned_in_a_window_is_not_the_calls() {
    explore(
        staged_directory(),
        vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let place = (tree.id("s/t"), tree.id("s"), hash_string("t"));
                let adding = tree.clone();
                let added = async move {
                    let added = lore_base::lore_spawn!(hooks::inherit(async move {
                        ops::staged_add(&adding, adding.id("a"), file("n"), "n").await
                    }));
                    let added = added.await.expect("the spawned add ends");
                    assert!(added.is_some(), "'a/n' adds");
                };
                assert!(ops::pend(&tree, place, added).await, "'s/t' is detached");
            })
        })],
        vec![],
        |run| {
            let [pend, add] = run.calls.as_slice() else {
                panic!("two calls: {:#?}", run.calls);
            };
            assert!(std::ptr::eq(pend.kind, &catalogue::PEND));
            let task = add.task.expect("the add's task");
            assert_ne!(task, 0, "the spawned future's own task");
            assert_eq!(
                run.spawns[task].call, None,
                "spawned in no call's own steps"
            );
            let made = run.of_op(0);
            assert!(made.iter().any(|record| record.task == Some(task)));
            for record in &made {
                let holding = [pend, add]
                    .iter()
                    .filter(|call| call.holds(record, &run.spawns))
                    .count();
                assert_eq!(holding, 1, "{record:?} is one call's");
            }
        },
    );
}

/// A settle of a directory unequal to its commit, beside a move of the directory, answers where
/// the move left it: not begun where it landed first, displaced where it landed once the settle
/// began, unequal where it landed after the settle.
#[test]
fn a_settle_beside_a_move_of_its_directory_answers_where_it_stands() {
    let displaced = Rc::new(Cell::new(0));
    let counts = displaced.clone();
    explore_with(
        Explore {
            reader_points: true,
            outside: &[1],
            ..Explore::default()
        },
        Box::new(|| {
            Box::pin(async {
                let tree = Tree::new(&["d/f", "e/"]).await;
                tree.state.mark_served();
                assert!(ops::served_create(&tree, tree.id("d"), "n").await.is_some());
                tree
            })
        }),
        vec![
            Box::new(|t: Arc<Tree>| {
                Box::pin(async move {
                    ops::settle(&t, t.id("d")).await;
                })
            }),
            Box::new(|t: Arc<Tree>| {
                Box::pin(async move {
                    let moved = ops::served_reattach_any(&t, t.id("d"), t.id("e"), "d").await;
                    assert_eq!(moved, Some(true), "'d' moves");
                })
            }),
        ],
        vec![],
        move |run| {
            let [settle] = run.calls.as_slice() else {
                panic!("one call: {:#?}", run.calls);
            };
            match settle.outcome {
                Outcome::Refused(Refusal::Displaced) => counts.set(counts.get() + 1),
                Outcome::Refused(Refusal::Unequal | Refusal::NotBegun) => {}
                ref other => panic!("the settle answered {other:?}"),
            }
        },
    );
    assert!(
        displaced.get() > 0,
        "no run moved the directory after the clear"
    );
}

/// A chaos runtime yielding at every point, its log off, with a tree of `paths` built on it.
fn chaos_tree(
    paths: &'static [&'static str],
) -> (tokio::runtime::Runtime, Arc<chaos::Chaos>, Arc<Tree>) {
    let (runtime, chaos) = chaos::runtime(2, 1, 7);
    let tree = runtime
        .block_on(lore_base::runtime::LORE_CONTEXT.scope(fixtures::execution(), Tree::new(paths)));
    (runtime, chaos, Arc::new(tree))
}

/// `future` run as task 0 on a chaos worker, to its end.
fn on_worker(runtime: &tokio::runtime::Runtime, future: impl Future<Output = ()> + Send + 'static) {
    runtime
        .block_on(
            runtime.spawn(
                lore_base::runtime::LORE_CONTEXT
                    .scope(fixtures::execution(), hooks::as_task(0, future)),
            ),
        )
        .expect("the worker's task ends");
}

/// A chaos worker's calls go to its runtime's log while the log records, and nowhere while it
/// does not, as a driver turns it on for a round.
#[test]
fn a_chaos_worker_keeps_calls_only_while_its_log_records() {
    let (runtime, chaos, tree) = chaos_tree(&["d/f"]);
    let marked = tree.clone();
    on_worker(&runtime, async move {
        ops::scan_mark_modified(&marked, marked.id("d/f")).await;
    });
    assert!(
        chaos.log().take_calls().is_empty(),
        "no call kept while the log is off"
    );
    chaos.log().set_recording(true);
    on_worker(&runtime, async move {
        ops::scan_mark_modified(&tree, tree.id("d/f")).await;
    });
    chaos.log().set_recording(false);
    let calls = chaos.log().take_calls();
    let [call] = calls.as_slice() else {
        panic!("one call: {calls:#?}");
    };
    assert!(std::ptr::eq(call.kind, &catalogue::SCAN_MARK_MODIFIED));
    assert_eq!(
        (call.task, &call.outcome),
        (Some(0), &Outcome::Done(Answer::None))
    );
    assert!(call.begin < call.end, "the mark took a seq inside its call");
    assert_eq!(chaos.log().no_task(), None);
}

/// A trusted move meeting a node gone once its node moved answers a failure, not the node gone:
/// the stage's move over a holder a detach took first, at the holder's deletion, and the dirty
/// move into a directory a detach took, at the destination's climb. No caller in the moves'
/// contract meets either, so each runs on a chaos worker, outside the model, whose replay rejects
/// a failure no failed read explains.
#[test]
fn a_trusted_move_meeting_a_node_gone_once_it_moved_answers_a_failure() {
    let over_a_holder = after_detach(&["p/x", "q/r"], "q/r", |tree| {
        Box::pin(async move {
            let (x, q, r) = (tree.id("p/x"), tree.id("q"), tree.id("q/r"));
            let moved = ops::stage_move(&tree, x, q, NameChange::Rename("r"), Some(r)).await;
            assert!(moved.is_err(), "the holder's deletion finds it gone");
        })
    });
    let into_a_directory = after_detach(&["p/x", "q/"], "q", |tree| {
        Box::pin(async move {
            let (x, q) = (tree.id("p/x"), tree.id("q"));
            let moved = ops::dirty_move(&tree, x, q, NameChange::Keep).await;
            assert!(moved.is_err(), "the destination's climb finds it gone");
        })
    });
    for (call, kind) in [
        (over_a_holder, &catalogue::STAGE_MOVE),
        (into_a_directory, &catalogue::DIRTY_MOVE),
    ] {
        assert!(
            std::ptr::eq(call.kind, kind),
            "a call of '{}'",
            call.kind.name
        );
        assert!(
            matches!(call.outcome, Outcome::Failed(_)),
            "'{}': {:?}",
            kind.name,
            call.outcome
        );
    }
}

/// The one call `op` makes, run as task 0 on a recording chaos worker of a runtime whose tree holds
/// `paths`, once an unserved detach took `gone`.
fn after_detach(
    paths: &'static [&'static str],
    gone: &'static str,
    op: impl FnOnce(Arc<Tree>) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>>,
) -> Call {
    let (runtime, chaos, tree) = chaos_tree(paths);
    let detached = runtime.block_on(lore_base::runtime::LORE_CONTEXT.scope(
        fixtures::execution(),
        ops::unserved_detach(&tree, tree.id(gone)),
    ));
    assert!(detached, "'{gone}' is detached");
    chaos.log().set_recording(true);
    on_worker(&runtime, op(tree));
    chaos.log().set_recording(false);
    let mut calls = chaos.log().take_calls();
    assert_eq!(calls.len(), 1, "one call: {calls:#?}");
    calls.remove(0)
}

/// `ops`, run as task 0 on a recording chaos worker of a runtime whose tree holds directories `s`
/// and `a` and the directory `s/t` a C API add staged; then its calls and records, the log
/// faultless.
fn on_chaos_with_staged_directory(
    ops: impl FnOnce(Arc<Tree>, NodeID) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>>,
) -> (Vec<Call>, Vec<Record>) {
    let (runtime, chaos, tree) = chaos_tree(&["s/", "a/"]);
    let t = runtime
        .block_on(lore_base::runtime::LORE_CONTEXT.scope(
            fixtures::execution(),
            ops::staged_add(&tree, tree.id("s"), directory("t"), "t"),
        ))
        .expect("'s/t' adds");
    chaos.log().set_recording(true);
    on_worker(&runtime, ops(tree, t));
    chaos.log().set_recording(false);
    let calls = chaos.log().take_calls();
    let records = chaos.log().drain();
    assert_eq!(chaos.log().no_task(), None);
    (calls, records)
}

/// On chaos workers too, a call made in a window holds its records and the call whose window it
/// is does not, wherever the workers ran each step.
#[test]
fn a_call_in_a_window_is_its_own_on_chaos_workers() {
    let (calls, records) = on_chaos_with_staged_directory(|tree, t| {
        Box::pin(async move {
            let place = (t, tree.id("s"), hash_string("t"));
            let added = async {
                let added = ops::staged_add(&tree, tree.id("a"), file("n"), "n").await;
                assert!(added.is_some(), "'a/n' adds");
            };
            assert!(ops::pend(&tree, place, added).await, "'s/t' is detached");
        })
    });
    let [pend, add] = calls.as_slice() else {
        panic!("two calls: {calls:#?}");
    };
    let made: Vec<&Record> = records
        .iter()
        .filter(|record| record.state == pend.state && record.task == Some(0))
        .collect();
    assert!(
        made.iter().any(|record| add.holds(record, &[])),
        "the add recorded"
    );
    for record in made {
        let holding = [pend, add]
            .iter()
            .filter(|call| call.holds(record, &[]))
            .count();
        assert_eq!(holding, 1, "{record:?} is one call's: {pend:?} {add:?}");
    }
}

/// A call its own task drops, as a timeout drops a request, ends as it drops, its drop glue inside
/// it: the task's next call follows it.
#[test]
fn a_call_dropped_by_its_task_ends_as_it_drops() {
    let (calls, records) = on_chaos_with_staged_directory(|tree, t| {
        Box::pin(async move {
            let place = (t, tree.id("s"), hash_string("t"));
            let pend = ops::pend(&tree, place, std::future::pending());
            let waited = tokio::time::timeout(Duration::from_millis(20), pend).await;
            assert!(waited.is_err(), "the pend waits on its window");
            let added = ops::staged_add(&tree, tree.id("a"), file("n"), "n").await;
            assert!(added.is_some(), "'a/n' adds");
        })
    });
    let [pend, add] = calls.as_slice() else {
        panic!("two calls: {calls:#?}");
    };
    assert_eq!(pend.outcome, Outcome::Dropped);
    assert!(pend.end <= add.begin, "{pend:?} then {add:?}");
    let released = records
        .iter()
        .find(|record| record.state == pend.state && matches!(record.effect, Effect::Unpermit))
        .expect("the pend's permit released");
    assert!(pend.holds(released, &[]), "{released:?} is {pend:?}'s");
    assert!(!add.holds(released, &[]));
}

/// A call made by no task of the run's is a fault: its records could name no operation.
#[test]
fn a_call_with_no_task_is_a_fault() {
    let (runtime, chaos, tree) = chaos_tree(&["d/f"]);
    chaos.log().set_recording(true);
    runtime
        .block_on(runtime.spawn(lore_base::runtime::LORE_CONTEXT.scope(
            fixtures::execution(),
            async move {
                ops::scan_mark_modified(&tree, tree.id("d/f")).await;
            },
        )))
        .expect("the task ends");
    chaos.log().set_recording(false);
    let _ = chaos.log().take_calls();
    let fault = chaos.log().no_task().unwrap_or_default();
    // The call's flag, raised at its first poll, before its records raise theirs.
    assert!(
        fault.starts_with("a call of 'scan mark' with no task"),
        "{fault:?}"
    );
}

/// Two calls one task makes at once overlap, which the log takes for a fault: their records
/// cannot be told apart. The first move yields at its points holding the moves permit, so the
/// second begins while the first is open.
#[test]
fn overlapping_calls_of_one_task_are_a_fault() {
    let (runtime, chaos, tree) = chaos_tree(&["a/x", "b/y"]);
    chaos.log().set_recording(true);
    on_worker(&runtime, async move {
        tokio::join!(
            ops::staged_move(&tree, tree.id("a/x"), tree.id("b"), "x"),
            ops::staged_move(&tree, tree.id("b/y"), tree.id("a"), "y"),
        );
    });
    chaos.log().set_recording(false);
    let calls = chaos.log().take_calls();
    assert_eq!(calls.len(), 2, "{calls:#?}");
    let fault = chaos.log().no_task().unwrap_or_default();
    assert!(fault.contains("overlapping calls"), "{fault:?}");
}

/// A call never ended nor dropped, as a future forgotten part way, is still open at quiescence:
/// a fault.
#[test]
fn a_call_still_open_at_quiescence_is_a_fault() {
    let (runtime, chaos, tree) = chaos_tree(&["s/", "a/"]);
    let t = runtime
        .block_on(lore_base::runtime::LORE_CONTEXT.scope(
            fixtures::execution(),
            ops::staged_add(&tree, tree.id("s"), directory("t"), "t"),
        ))
        .expect("'s/t' adds");
    chaos.log().set_recording(true);
    on_worker(&runtime, async move {
        let place = (t, tree.id("s"), hash_string("t"));
        let mut pend = Box::pin(ops::pend(&tree, place, std::future::pending()));
        let waited = tokio::time::timeout(Duration::from_millis(20), &mut pend).await;
        assert!(waited.is_err(), "the pend waits on its window");
        #[expect(
            clippy::mem_forget,
            reason = "a call neither ended nor dropped is what the test leaves"
        )]
        std::mem::forget(pend);
    });
    chaos.log().set_recording(false);
    let _ = chaos.log().take_calls();
    let fault = chaos.log().no_task().unwrap_or_default();
    assert!(fault.contains("still open"), "{fault:?}");
}

/// A checked create of a name a file holds, beside another task's detach of the file or its rename
/// in place: the create answers the held name where it found the holder, a live child of its
/// directory at some `seq` of its call, though taken or renamed before the call ends, noting the
/// holder taken where its settle found it gone; or attaches where the holder had gone. The rename
/// changes the holder's hash in place, which no caller does beside a VFS provider diff's create, as
/// the respellings keep it: it covers what the create answers of a holder renamed away. Each run
/// replays clean with its calls.
#[test]
fn a_checked_create_of_a_held_name_beside_a_detach_or_a_rename_of_its_holder() {
    let take: [(OpFn, bool); 2] = [
        (
            |tree| {
                Box::pin(async move { assert!(ops::unserved_detach(&tree, tree.id("d/f")).await) })
            },
            true,
        ),
        (
            |tree| Box::pin(async move { ops::rename_then_mark(&tree, tree.id("d/f"), "g").await }),
            false,
        ),
    ];
    for (taken, detaches) in take {
        let (within, attached) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
        let (counts_within, counts_attached) = (within.clone(), attached.clone());
        let noted = Rc::new(Cell::new(0));
        let counts_noted = noted.clone();
        explore(
            tree(&["d/f"]),
            vec![
                Box::new(|tree: Arc<Tree>| {
                    Box::pin(async move {
                        let _ = ops::checked_create(&tree, tree.id("d"), "f").await;
                    })
                }),
                Box::new(taken),
            ],
            vec![],
            move |run| {
                let f = run.tree.id("d/f");
                let create = run
                    .calls
                    .iter()
                    .find(|call| std::ptr::eq(call.kind, &catalogue::CHECKED_CREATE))
                    .expect("the create's call");
                match create.outcome {
                    Outcome::Done(Answer::Held(holder)) => {
                        assert_eq!(holder, f, "the create found 'd/f'");
                        let taken_within = run.of_op(1).iter().any(|record| {
                            matches!(
                                record.effect,
                                Effect::Detach { node, .. } | Effect::Rename { node, .. }
                                    if node == f
                            ) && (create.begin..create.end).contains(&record.seq)
                        });
                        if taken_within {
                            counts_within.set(counts_within.get() + 1);
                        }
                        if create.taken.iter().any(|&(node, _)| node == f) {
                            counts_noted.set(counts_noted.get() + 1);
                        }
                    }
                    Outcome::Done(Answer::Node(_)) => {
                        counts_attached.set(counts_attached.get() + 1);
                    }
                    ref other => panic!("the create answered {other:?}"),
                }
            },
        );
        assert!(
            within.get() > 0 && attached.get() > 0,
            "held and taken within the create {}, attached {}",
            within.get(),
            attached.get()
        );
        assert_eq!(
            noted.get() > 0,
            detaches,
            "the holder noted taken in {} runs",
            noted.get()
        );
    }
}

/// Two checked creates of one name in one directory: one attaches, the other answers the name
/// held by the node the first published while it ran; each run replays clean with its calls.
#[test]
fn two_checked_creates_of_one_name() {
    let raced = Rc::new(Cell::new(0));
    let counts = raced.clone();
    let create: OpFn = |tree| {
        Box::pin(async move {
            let _ = ops::checked_create(&tree, tree.id("d"), "n").await;
        })
    };
    explore(
        tree(&["d/"]),
        vec![Box::new(create), Box::new(create)],
        vec![],
        move |run| {
            for call in &run.calls {
                let Outcome::Done(Answer::Held(holder)) = call.outcome else {
                    continue;
                };
                let published_within = run.records.iter().any(|record| {
                    matches!(record.effect, Effect::Publish { node, .. } if node == holder)
                        && (call.begin..call.end).contains(&record.seq)
                });
                if published_within {
                    counts.set(counts.get() + 1);
                }
            }
        },
    );
    assert!(
        raced.get() > 0,
        "some create answered a name the other published while it ran"
    );
}

/// An unserved add, a scan's, a checked create, a scan's chain, a VFS provider diff's or `lore
/// dirty`'s, beside a detach of its directory: the add answers the directory gone where the
/// detach came first, by its not-found or by the directory `Discarded` under the add's lock, and
/// its node where it published first, the directory taken before its mark, or the node, for
/// `lore dirty`, which marks its node; each run replays clean with its calls.
#[test]
fn an_add_beside_a_detach_of_its_directory() {
    // Each add, and whether it marks its node rather than the directory.
    let adds: [(OpFn, bool); 5] = [
        (
            |tree| Box::pin(async move { ops::scan_add(&tree, tree.id("d"), "n").await }),
            false,
        ),
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::checked_create(&tree, tree.id("d"), "n").await;
                })
            },
            false,
        ),
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::scan_chain(&tree, tree.id("d"), "n").await;
                })
            },
            false,
        ),
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::diff_add(&tree, tree.id("d"), "n", true).await;
                })
            },
            false,
        ),
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::dirty_add(&tree, tree.id("d"), "", "n", true).await;
                })
            },
            true,
        ),
    ];
    for (add, marks_node) in adds {
        let (gone, taken) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
        let (counts_gone, counts_taken) = (gone.clone(), taken.clone());
        let noted = Rc::new(Cell::new(0));
        let counts_noted = noted.clone();
        explore(
            tree(&["d/f"]),
            vec![
                Box::new(add),
                Box::new(|tree: Arc<Tree>| {
                    Box::pin(
                        async move { assert!(ops::unserved_detach(&tree, tree.id("d")).await) },
                    )
                }),
            ],
            vec![],
            move |run| {
                let d = run.tree.id("d");
                let add = run
                    .calls
                    .iter()
                    .find(|call| !std::ptr::eq(call.kind, &catalogue::UNSERVED_DETACH))
                    .expect("the add's call");
                match add.outcome {
                    Outcome::Refused(Refusal::NotFound) => counts_gone.set(counts_gone.get() + 1),
                    Outcome::Done(Answer::Node(added)) => {
                        let published =
                            run.of_op(0).iter().find_map(|record| match record.effect {
                                Effect::Publish { .. } => Some(record.seq),
                                _ => None,
                            });
                        let detached = run.of_op(1).iter().find_map(|record| match record.effect {
                            Effect::Detach { node, .. } if node == d => Some(record.seq),
                            _ => None,
                        });
                        if let (Some(published), Some(detached)) = (published, detached)
                            && published < detached
                            && detached < add.end
                        {
                            counts_taken.set(counts_taken.get() + 1);
                        }
                        let marked = if marks_node { added } else { d };
                        if add.taken.iter().any(|&(node, _)| node == marked) {
                            counts_noted.set(counts_noted.get() + 1);
                        }
                    }
                    ref other => panic!("the add answered {other:?}"),
                }
            },
        );
        assert!(
            gone.get() > 0 && taken.get() > 0 && noted.get() > 0,
            "gone {}, taken after its publish {}, noted taken {}",
            gone.get(),
            taken.get(),
            noted.get()
        );
    }
}

/// A scan's chain or `lore dirty`'s add from the root through `d`, beside a detach of `d`: where
/// the detach came after its find of `d` and before its add into `d`, the call answers it
/// published nothing, `d` noted taken; where after, the directory or node it published last, a
/// node noted taken where the detach came before a mark, and nothing published after. A chain
/// whose find came after the detach adds `d` again, as its caller adds a name it found missing.
/// Each run replays clean with its calls.
#[test]
fn a_chain_beside_a_detach_of_a_directory_it_found() {
    let chains: [OpFn; 2] = [
        |tree| {
            Box::pin(async move {
                let _ = ops::scan_chain(&tree, ROOT_NODE, "d/n1/n2").await;
            })
        },
        |tree| {
            Box::pin(async move {
                let _ = ops::dirty_add(&tree, ROOT_NODE, "d/m", "n", true).await;
            })
        },
    ];
    for chain in chains {
        let (nothing, taken) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
        let (counts_nothing, counts_taken) = (nothing.clone(), taken.clone());
        let again = Rc::new(Cell::new(0));
        let counts_again = again.clone();
        explore(
            tree(&["d/f"]),
            vec![
                Box::new(chain),
                Box::new(|tree: Arc<Tree>| {
                    Box::pin(
                        async move { assert!(ops::unserved_detach(&tree, tree.id("d")).await) },
                    )
                }),
            ],
            vec![],
            move |run| {
                let d = run.tree.id("d");
                let chain = run
                    .calls
                    .iter()
                    .find(|call| !std::ptr::eq(call.kind, &catalogue::UNSERVED_DETACH))
                    .expect("the chain's call");
                let added_again = run.of_op(0).iter().any(|record| {
                    matches!(record.effect, Effect::Publish { parent, name_hash, .. }
                        if parent == ROOT_NODE && name_hash == hash_string("d"))
                });
                if added_again {
                    counts_again.set(counts_again.get() + 1);
                }
                // Its callers stop at the error a taken node answers, so the step that found one
                // is its last.
                if let Some(&(_, from)) = chain.taken.first() {
                    assert!(
                        !run.of_op(0).iter().any(|record| {
                            matches!(record.effect, Effect::Publish { .. }) && record.seq > from
                        }),
                        "the chain stops where it found a node taken"
                    );
                }
                match chain.outcome {
                    Outcome::Done(Answer::None) => {
                        assert!(
                            chain.taken.iter().any(|&(node, _)| node == d),
                            "a chain that published nothing found 'd' taken"
                        );
                        counts_nothing.set(counts_nothing.get() + 1);
                    }
                    Outcome::Done(Answer::Node(_)) => {
                        if !chain.taken.is_empty() {
                            counts_taken.set(counts_taken.get() + 1);
                        }
                    }
                    ref other => panic!("the chain answered {other:?}"),
                }
            },
        );
        assert!(
            nothing.get() > 0 && taken.get() > 0 && again.get() > 0,
            "published nothing {}, noted taken after a publish {}, added 'd' again {}",
            nothing.get(),
            taken.get(),
            again.get()
        );
    }
}

/// Two path-filtered scans' chains from the root through `x`, which status made first as a
/// directory their paths share, each adding its own directory into it: each publishes its own and
/// marks `x`, closure kept and the names unique; each run replays clean with its calls.
#[test]
fn two_scan_chains_into_one_directory() {
    let chains: Vec<Op<Tree>> = ["x/n1", "x/n2/m"]
        .into_iter()
        .map(|names| -> Op<Tree> {
            Box::new(move |tree: Arc<Tree>| {
                Box::pin(async move {
                    let chained = ops::scan_chain(&tree, ROOT_NODE, names).await;
                    assert!(chained.is_some(), "the chain to '{names}' is made");
                })
            })
        })
        .collect();
    let closed = Rules {
        dirty_closure: true,
        unique_names: true,
        ..Rules::default()
    };
    explore_with(
        Explore {
            rules: Some(closed),
            ..Explore::default()
        },
        tree(&["x/f1"]),
        chains,
        vec![],
        |run| {
            for call in &run.calls {
                assert!(
                    matches!(call.outcome, Outcome::Done(Answer::Node(_))),
                    "each chain answers what it added: {:?}",
                    call.outcome
                );
            }
        },
    );
}

/// A served mark of a directory beside a settle of it: a mark that answers done claims the
/// directory admitted a served change at some `seq` of its call, though the settle's phases take
/// that from it before the call ends; each run replays clean with its calls.
#[test]
fn a_served_mark_beside_a_settle_of_its_directory() {
    let taken = Rc::new(Cell::new(0));
    let counts = taken.clone();
    let setup: Setup<Tree> = Box::new(|| {
        Box::pin(async {
            let tree = Tree::new(&["a/d/f", "a/d/e/"]).await;
            tree.state.mark_served();
            for dir in ["a", "a/d"] {
                assert!(ops::served_mark_dir(&tree, tree.id(dir)).await);
            }
            tree
        })
    });
    explore(
        setup,
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    ops::served_mark_dir(&tree, tree.id("a/d")).await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    ops::settle(&tree, tree.id("a/d")).await;
                })
            }),
        ],
        vec![],
        move |run| {
            let d = run.tree.id("a/d");
            let mark = run
                .calls
                .iter()
                .find(|call| std::ptr::eq(call.kind, &catalogue::SERVED_MARK))
                .expect("the mark's call");
            if mark.outcome != Outcome::Done(Answer::None) {
                return;
            }
            let settled_within = run.of_op(1).iter().any(|record| {
                matches!(
                    &record.effect,
                    Effect::Word { node, record: Words(is), clear_pending }
                        if *node == d && (*clear_pending || !is.is_dirty())
                ) && (mark.begin..mark.end).contains(&record.seq)
            });
            if settled_within {
                counts.set(counts.get() + 1);
            }
        },
    );
    assert!(
        taken.get() > 0,
        "some mark answered done while the settle took the directory's admission within it"
    );
}

/// A C API move of a file beside the C API delete of it, one after the other under the moves
/// permit: the move refuses a file the delete staged first, and the delete's level refuses a file
/// the move took from its parent first; each run replays clean with its calls, the refusals held
/// to the model at some `seq` of their calls.
#[test]
fn a_staged_move_and_a_delete_of_its_file() {
    let (move_refused, level_refused) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
    let (counts_move, counts_level) = (move_refused.clone(), level_refused.clone());
    let ops: [OpFn; 2] = [
        |tree| {
            Box::pin(
                async move { ops::staged_move(&tree, tree.id("d/f"), tree.id("e"), "f").await },
            )
        },
        |tree| {
            Box::pin(
                async move { ops::staged_delete_level(&tree, tree.id("d/f"), tree.id("d")).await },
            )
        },
    ];
    explore(
        tree(&["d/f", "e/"]),
        ops.into_iter().map(|op| Box::new(op) as Op<Tree>).collect(),
        vec![],
        move |run| {
            for call in &run.calls {
                match call.outcome {
                    Outcome::Refused(Refusal::Move) => counts_move.set(counts_move.get() + 1),
                    Outcome::Refused(Refusal::DeleteLevelOther) => {
                        counts_level.set(counts_level.get() + 1);
                    }
                    _ => {}
                }
            }
        },
    );
    assert!(
        move_refused.get() > 0 && level_refused.get() > 0,
        "the move refused {}, the level {}",
        move_refused.get(),
        level_refused.get()
    );
}

/// Two served renames of directories holding local content, each into the other: the one that
/// moves second refuses a move into its own subtree, held to the model at some `seq` of its call.
#[test]
fn two_served_renames_of_directories_into_each_other() {
    let refused = Rc::new(Cell::new(0));
    let counts = refused.clone();
    let setup: Setup<Tree> = Box::new(|| {
        Box::pin(async {
            let tree = Tree::new(&["a/", "b/"]).await;
            tree.state.mark_served();
            for dir in ["a", "b"] {
                assert!(ops::served_mark_dir(&tree, tree.id(dir)).await);
                ops::hold_local(&tree, tree.id(dir)).await;
            }
            tree
        })
    });
    let ops: [OpFn; 2] = [
        |tree| {
            Box::pin(async move {
                ops::served_rename(&tree, tree.id("a"), tree.id("b"), "a").await;
            })
        },
        |tree| {
            Box::pin(async move {
                ops::served_rename(&tree, tree.id("b"), tree.id("a"), "b").await;
            })
        },
    ];
    explore(
        setup,
        ops.into_iter().map(|op| Box::new(op) as Op<Tree>).collect(),
        vec![],
        move |run| {
            if run
                .calls
                .iter()
                .any(|call| call.outcome == Outcome::Refused(Refusal::Subtree))
            {
                counts.set(counts.get() + 1);
            }
        },
    );
    assert!(
        refused.get() > 0,
        "no rename refused a move into its own subtree"
    );
}

/// A C API add into a directory beside the C API delete of the directory's level, or beside an
/// unserved detach of it: the add refuses a directory the delete staged first, and answers not
/// found for one the detach took first, `Discarded`, each held to the model at some `seq` of its
/// call; each run replays clean with its calls.
#[test]
fn a_staged_add_beside_a_delete_or_a_detach_of_its_directory() {
    let takes: [(OpFn, Refusal); 2] = [
        (
            |tree| {
                Box::pin(
                    async move { ops::staged_delete_level(&tree, tree.id("d"), ROOT_NODE).await },
                )
            },
            Refusal::Add,
        ),
        (
            |tree| {
                Box::pin(async move { assert!(ops::unserved_detach(&tree, tree.id("d")).await) })
            },
            Refusal::NotFound,
        ),
    ];
    for (take, reason) in takes {
        let refused = Rc::new(Cell::new(0));
        let counts = refused.clone();
        explore(
            tree(&["d/"]),
            vec![
                Box::new(|tree: Arc<Tree>| {
                    Box::pin(async move {
                        let _ = ops::staged_add(&tree, tree.id("d"), file("n"), "n").await;
                    })
                }),
                Box::new(take),
            ],
            vec![],
            move |run| {
                let refusing = |call: &Call| {
                    std::ptr::eq(call.kind, &catalogue::STAGED_ADD)
                        && call.outcome == Outcome::Refused(reason)
                };
                if run.calls.iter().any(refusing) {
                    counts.set(counts.get() + 1);
                }
            },
        );
        assert!(refused.get() > 0, "no add refused {reason:?}");
    }
}

/// A C API restore of a file staged for deletion beside the C API delete of its directory's level,
/// and a C API modify of a file beside the delete of the file's: the restore refuses below a
/// directory the delete staged first (H18), the modify a file the delete staged first, each held
/// to the model at some `seq` of its call; each run replays clean with its calls.
#[test]
fn a_restore_or_a_modify_beside_a_delete() {
    let cases: [(Setup<Tree>, OpFn, OpFn, Refusal); 2] = [
        (
            changed(&["d/f"], |t| {
                Box::pin(ops::staged_delete_level(t, t.id("d/f"), t.id("d")))
            }),
            |tree| {
                Box::pin(async move {
                    let _ = ops::restore(&tree, tree.id("d/f"), &Content::default()).await;
                })
            },
            |tree| {
                Box::pin(
                    async move { ops::staged_delete_level(&tree, tree.id("d"), ROOT_NODE).await },
                )
            },
            Refusal::Restore,
        ),
        (
            tree(&["d/f"]),
            |tree| {
                Box::pin(async move {
                    let content = Content {
                        size: 3,
                        ..Content::default()
                    };
                    let _ = ops::staged_modify(&tree, tree.id("d/f"), &content).await;
                })
            },
            |tree| {
                Box::pin(async move {
                    ops::staged_delete_level(&tree, tree.id("d/f"), tree.id("d")).await;
                })
            },
            Refusal::Modify,
        ),
    ];
    for (setup, change, delete, reason) in cases {
        let refused = Rc::new(Cell::new(0));
        let counts = refused.clone();
        explore(
            setup,
            vec![Box::new(change), Box::new(delete)],
            vec![],
            move |run| {
                if run
                    .calls
                    .iter()
                    .any(|call| call.outcome == Outcome::Refused(reason))
                {
                    counts.set(counts.get() + 1);
                }
            },
        );
        assert!(refused.get() > 0, "nothing refused {reason:?}");
    }
}

/// A dirty rewrite of a file beside a detach of its directory: the rewrite's mark of the directory
/// finds it taken where the detach landed after the rewrite, noted on the call and held gone from
/// there; each run replays clean with its calls.
#[test]
fn a_dirty_rewrite_beside_a_detach_of_its_directory() {
    let noted = Rc::new(Cell::new(0));
    let counts = noted.clone();
    explore(
        tree(&["d/f"]),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move {
                    let content = Content {
                        size: 3,
                        ..Content::default()
                    };
                    ops::dirty_rewrite(&tree, tree.id("d/f"), tree.id("d"), &content).await;
                })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move { assert!(ops::unserved_detach(&tree, tree.id("d")).await) })
            }),
        ],
        vec![],
        move |run| {
            let d = run.tree.id("d");
            let noting = |call: &Call| {
                std::ptr::eq(call.kind, &catalogue::DIRTY_REWRITE)
                    && call.taken.iter().any(|&(node, _)| node == d)
            };
            if run.calls.iter().any(noting) {
                counts.set(counts.get() + 1);
            }
        },
    );
    assert!(noted.get() > 0, "no rewrite noted its directory taken");
}

/// Whether a call other than `call` wrote `call`'s target strictly between `call`'s own last write
/// there and its end: a done answer's words overwritten inside its interval.
fn overwritten_within(run: &Run<'_>, call: &Call) -> bool {
    let Some(target) = call.target else {
        return false;
    };
    let on_target = |record: &&Record| {
        matches!(
            record.effect,
            Effect::Word { node, .. } | Effect::Move { node, .. } | Effect::Rename { node, .. }
                if node == target
        )
    };
    let own = run
        .records
        .iter()
        .filter(on_target)
        .filter(|record| call.holds(record, &run.spawns))
        .map(|record| record.seq)
        .max();
    own.is_some_and(|own| {
        run.records.iter().filter(on_target).any(|record| {
            own < record.seq && record.seq < call.end && !call.holds(record, &run.spawns)
        })
    })
}

/// The pre-serving respelling of `d/f` to `F` beside an unserved detach of it: gone where the
/// detach landed before the rename, done otherwise, with the node noted taken where the detach
/// landed between the rename and its mark. Each run replays clean with its calls.
#[test]
fn a_rename_beside_a_detach_of_its_node_answers_done_or_gone() {
    let (gone, taken) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
    let (counts_gone, counts_taken) = (gone.clone(), taken.clone());
    explore(
        tree(&["d/f"]),
        vec![
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move { ops::rename_then_mark(&tree, tree.id("d/f"), "F").await })
            }),
            Box::new(|tree: Arc<Tree>| {
                Box::pin(async move { assert!(ops::unserved_detach(&tree, tree.id("d/f")).await) })
            }),
        ],
        vec![],
        move |run| {
            let f = run.tree.id("d/f");
            let rename = (run.calls.iter())
                .find(|call| std::ptr::eq(call.kind, &catalogue::RENAME))
                .expect("a call of the rename");
            match &rename.outcome {
                outcome if *outcome == Outcome::Refused(Refusal::NotFound) => {
                    counts_gone.set(counts_gone.get() + 1);
                }
                Outcome::Done(Answer::None) => {
                    if rename.taken.iter().any(|&(node, _)| node == f) {
                        counts_taken.set(counts_taken.get() + 1);
                    }
                }
                outcome => panic!("the rename answered {outcome:?}"),
            }
        },
    );
    assert!(gone.get() > 0, "no rename found its node gone");
    assert!(
        taken.get() > 0,
        "no rename noted its node taken before its mark"
    );
}

/// Two changes of one file whose done answers leave contrary words, the second landing after the
/// first's own write and before it ends: a scan's mark and its clear, a dirty rewrite and an
/// in-place change of the size, a C API restore and the delete of the file again, two
/// respellings to two spellings, each yielding after its own write (the rewrite at its parent's
/// mark, the restore at its climb, the respelling at its own mark). Each done answer held to its
/// words where its own write left them; each run replays clean with its calls.
#[test]
fn contrary_changes_of_one_file_each_leave_their_words() {
    let pairs: [(Setup<Tree>, OpFn, OpFn); 4] = [
        (
            tree(&["d/f"]),
            |tree| Box::pin(async move { ops::scan_mark_modified(&tree, tree.id("d/f")).await }),
            |tree| Box::pin(async move { ops::scan_clear(&tree, tree.id("d/f")).await }),
        ),
        (
            tree(&["d/f"]),
            |tree| {
                Box::pin(async move {
                    let content = Content {
                        size: 3,
                        ..Content::default()
                    };
                    ops::dirty_rewrite(&tree, tree.id("d/f"), tree.id("d"), &content).await;
                })
            },
            |tree| {
                Box::pin(async move {
                    let place = (tree.id("d"), hash_string("f"));
                    let _ = ops::in_place_size(&tree, tree.id("d/f"), place, 5).await;
                })
            },
        ),
        (
            changed(&["d/f"], |t| {
                Box::pin(ops::staged_delete_level(t, t.id("d/f"), t.id("d")))
            }),
            |tree| {
                Box::pin(async move {
                    let _ = ops::restore(&tree, tree.id("d/f"), &Content::default()).await;
                })
            },
            |tree| {
                Box::pin(async move {
                    ops::staged_delete_level(&tree, tree.id("d/f"), tree.id("d")).await;
                })
            },
        ),
        (
            tree(&["d/ab"]),
            |tree| {
                Box::pin(async move { ops::rename_then_mark(&tree, tree.id("d/ab"), "Ab").await })
            },
            |tree| {
                Box::pin(async move { ops::rename_then_mark(&tree, tree.id("d/ab"), "aB").await })
            },
        ),
    ];
    for (index, (setup, first, second)) in pairs.into_iter().enumerate() {
        let overwritten = Rc::new(Cell::new(0));
        let counts = overwritten.clone();
        explore(
            setup,
            vec![Box::new(first), Box::new(second)],
            vec![],
            move |run| {
                let reached = run.calls.iter().any(|call| {
                    call.outcome == Outcome::Done(Answer::None) && overwritten_within(run, call)
                });
                if reached {
                    counts.set(counts.get() + 1);
                }
            },
        );
        assert!(
            overwritten.get() > 0,
            "pair {index}: no done answer's words overwritten inside its call"
        );
    }
}

/// A C API delete's pend of an added directory beside a C API move of it, one after the other under
/// the moves permit: the pend refuses a directory the move took from its place first, live where
/// it landed, held to the model by the place the delete found it at; each run replays clean with
/// its calls.
#[test]
fn a_pend_beside_a_move_of_its_directory() {
    let refused = Rc::new(Cell::new(0));
    let counts = refused.clone();
    let ops: [OpFn; 2] = [
        |tree| {
            Box::pin(async move {
                let place = (tree.id("s/t"), tree.id("s"), hash_string("t"));
                let _ = ops::pend(&tree, place, async {}).await;
            })
        },
        |tree| {
            Box::pin(
                async move { ops::staged_move(&tree, tree.id("s/t"), tree.id("a"), "t").await },
            )
        },
    ];
    explore(
        staged_directory(),
        ops.into_iter().map(|op| Box::new(op) as Op<Tree>).collect(),
        vec![],
        move |run| {
            let refusing = |call: &Call| {
                std::ptr::eq(call.kind, &catalogue::PEND)
                    && call.outcome == Outcome::Refused(Refusal::NotFound)
            };
            if run.calls.iter().any(refusing) {
                counts.set(counts.get() + 1);
            }
        },
    );
    assert!(
        refused.get() > 0,
        "no pend refused its directory moved away"
    );
}

/// A C API delete of an added directory holding a file, then the commit's drain: the drain answers
/// the subtree it freed, as the model counts its frees.
#[test]
fn a_drain_answers_what_a_pend_queued() {
    let setup: Setup<Tree> = Box::new(|| {
        Box::pin(async {
            let mut tree = staged_directory()().await;
            let t = tree.id("s/t");
            let u = ops::staged_add(&tree, t, file("u"), "u")
                .await
                .expect("'s/t/u' adds");
            tree.nodes.push(("s/t/u".to_string(), u));
            tree
        })
    });
    explore(
        setup,
        vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let place = (tree.id("s/t"), tree.id("s"), hash_string("t"));
                assert!(ops::pend(&tree, place, async {}).await, "'s/t' is detached");
                ops::drain(&tree).await;
            })
        })],
        vec![],
        |run| {
            let [_, drain] = run.calls.as_slice() else {
                panic!("two calls: {:#?}", run.calls);
            };
            assert!(std::ptr::eq(drain.kind, &catalogue::DRAIN));
            assert_eq!(
                drain.outcome,
                Outcome::Done(Answer::Freed {
                    files: 1,
                    directories: 1,
                })
            );
        },
    );
}

/// The one call of `op` on the tree `setup` builds is of `kind` and answers as `expected` says, in
/// every run.
fn answers(
    what: &'static str,
    kind: &'static Kind,
    setup: Setup<Tree>,
    op: Op<Tree>,
    expected: impl Fn(&Outcome) -> bool + 'static,
) {
    explore(setup, vec![op], vec![], move |run| {
        let [call] = run.calls.as_slice() else {
            panic!("{what}: one call: {:#?}", run.calls);
        };
        assert!(
            std::ptr::eq(call.kind, kind),
            "{what}: a call of '{}'",
            call.kind.name
        );
        assert!(expected(&call.outcome), "{what}: {:?}", call.outcome);
    });
}

fn refused(reason: Refusal) -> impl Fn(&Outcome) -> bool {
    move |outcome| *outcome == Outcome::Refused(reason)
}

fn done(outcome: &Outcome) -> bool {
    *outcome == Outcome::Done(Answer::None)
}

/// A served tree of `paths` whose directory `d` a served mark left dirty, its children still the
/// committed ones.
fn marked(paths: &'static [&'static str]) -> Setup<Tree> {
    Box::new(move || {
        Box::pin(async move {
            let tree = Tree::new(paths).await;
            tree.state.mark_served();
            assert!(ops::served_mark_dir(&tree, tree.id("d")).await);
            tree
        })
    })
}

/// Each unserved op answers each outcome it can reach from a State that decides it, alone.
#[test]
fn each_unserved_op_answers_what_it_found() {
    answers(
        "a checked create over a held name",
        &catalogue::CHECKED_CREATE,
        tree(&["d/f"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let _ = ops::checked_create(&t, t.id("d"), "f").await;
            })
        }),
        |outcome| matches!(outcome, Outcome::Done(Answer::Held(_))),
    );
    answers(
        "a checked create in a directory gone",
        &catalogue::CHECKED_CREATE,
        changed(&["d/f", "e/"], |t| {
            Box::pin(async move { assert!(ops::unserved_detach(t, t.id("e")).await) })
        }),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let _ = ops::checked_create(&t, t.id("e"), "n").await;
            })
        }),
        refused(Refusal::NotFound),
    );
    answers(
        "a scan's chain along a path the State holds",
        &catalogue::SCAN_CHAIN,
        tree(&["d/f"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                assert_eq!(ops::scan_chain(&t, ROOT_NODE, "d").await, Some(t.id("d")));
            })
        }),
        done,
    );
    answers(
        "a scan's chain from a directory gone",
        &catalogue::SCAN_CHAIN,
        changed(&["d/f", "e/"], |t| {
            Box::pin(async move { assert!(ops::unserved_detach(t, t.id("e")).await) })
        }),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                assert_eq!(ops::scan_chain(&t, t.id("e"), "n").await, None);
            })
        }),
        refused(Refusal::NotFound),
    );
    answers(
        "a diff add in a directory gone",
        &catalogue::DIFF_ADD,
        changed(&["d/f", "e/"], |t| {
            Box::pin(async move { assert!(ops::unserved_detach(t, t.id("e")).await) })
        }),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                assert_eq!(ops::diff_add(&t, t.id("e"), "n", true).await, None);
            })
        }),
        refused(Refusal::NotFound),
    );
    answers(
        "a dirty add in a directory gone",
        &catalogue::DIRTY_ADD,
        changed(&["d/f", "e/"], |t| {
            Box::pin(async move { assert!(ops::unserved_detach(t, t.id("e")).await) })
        }),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                assert_eq!(ops::dirty_add(&t, t.id("e"), "", "n", true).await, None);
            })
        }),
        refused(Refusal::NotFound),
    );
    answers(
        "an unserved detach of a node gone",
        &catalogue::UNSERVED_DETACH,
        changed(&["d/f"], |t| {
            Box::pin(async move { assert!(ops::unserved_detach(t, t.id("d/f")).await) })
        }),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { assert!(!ops::unserved_detach(&t, t.id("d/f")).await) })
        }),
        refused(Refusal::NotFound),
    );
    answers(
        "a restore of a node not staged for deletion",
        &catalogue::RESTORE,
        tree(&["d/f"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                assert!(!ops::restore(&t, t.id("d/f"), &Content::default()).await);
            })
        }),
        refused(Refusal::Restore),
    );
    answers(
        "a modify of a file a delete staged",
        &catalogue::STAGED_MODIFY,
        changed(&["d/f"], |t| {
            Box::pin(ops::staged_delete_level(t, t.id("d/f"), t.id("d")))
        }),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                assert!(!ops::staged_modify(&t, t.id("d/f"), &Content::default()).await);
            })
        }),
        refused(Refusal::Modify),
    );
    answers(
        "a delete level of a node staged for deletion",
        &catalogue::STAGED_DELETE,
        changed(&["d/f"], |t| {
            Box::pin(ops::staged_delete_level(t, t.id("d/f"), t.id("d")))
        }),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { ops::staged_delete_level(&t, t.id("d/f"), t.id("d")).await })
        }),
        refused(Refusal::DeleteLevelStaged),
    );
    answers(
        "a delete level of a node added",
        &catalogue::STAGED_DELETE,
        staged_directory(),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { ops::staged_delete_level(&t, t.id("s/t"), t.id("s")).await })
        }),
        refused(Refusal::DeleteLevelOther),
    );
    answers(
        "a staged add",
        &catalogue::STAGED_ADD,
        tree(&["d/"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                assert!(
                    ops::staged_add(&t, t.id("d"), file("n"), "n")
                        .await
                        .is_some()
                );
            })
        }),
        |outcome| matches!(outcome, Outcome::Done(Answer::Node(_))),
    );
    answers(
        "an owned clear of a clean file",
        &catalogue::OWNED_CLEAR,
        tree(&["d/f"]),
        Box::new(|t: Arc<Tree>| Box::pin(async move { ops::owned_clear(&t, t.id("d/f")).await })),
        refused(Refusal::Declined),
    );
    answers(
        "an in-place change away from its place",
        &catalogue::IN_PLACE,
        tree(&["d/f", "e/"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let place = (t.id("e"), hash_string("f"));
                assert!(!ops::in_place_size(&t, t.id("d/f"), place, 3).await);
            })
        }),
        refused(Refusal::Declined),
    );
    answers(
        "a dirty move of a node gone",
        &catalogue::DIRTY_MOVE,
        x_gone(&["p/x", "q/"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let moved = ops::dirty_move(&t, t.id("p/x"), t.id("q"), NameChange::Keep).await;
                assert!(moved.is_err(), "the move finds 'p/x' gone");
            })
        }),
        refused(Refusal::NotFound),
    );
    answers(
        "a stage move of a node gone",
        &catalogue::STAGE_MOVE,
        x_gone(&["p/x", "q/"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let (x, q) = (t.id("p/x"), t.id("q"));
                let moved = ops::stage_move(&t, x, q, NameChange::Keep, None).await;
                assert!(moved.is_err(), "the move finds 'p/x' gone");
            })
        }),
        refused(Refusal::NotFound),
    );
    answers(
        "a realize move of a node gone",
        &catalogue::REALIZE_MOVE,
        x_gone(&["p/x", "q/"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let (x, q) = (t.id("p/x"), t.id("q"));
                let moved = ops::realize_move(&t, x, q, NameChange::Keep, false).await;
                assert!(moved.is_err(), "the move finds 'p/x' gone");
            })
        }),
        refused(Refusal::NotFound),
    );
    answers(
        "a stage respell of a node gone",
        &catalogue::STAGE_RESPELL,
        x_gone(&["p/x"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { ops::stage_respell(&t, t.id("p/x"), "X").await })
        }),
        refused(Refusal::NotFound),
    );
    answers(
        "a rename of a node gone",
        &catalogue::RENAME,
        x_gone(&["p/x"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { ops::rename_then_mark(&t, t.id("p/x"), "X").await })
        }),
        refused(Refusal::NotFound),
    );
    for (what, kind, to) in [
        ("a stage respell", &catalogue::STAGE_RESPELL, "X"),
        ("a stage respell spelled so", &catalogue::STAGE_RESPELL, "x"),
        ("a rename's respelling", &catalogue::RENAME, "X"),
        ("a rename spelled so", &catalogue::RENAME, "x"),
    ] {
        let respell: Op<Tree> = Box::new(move |t: Arc<Tree>| {
            Box::pin(async move {
                if std::ptr::eq(kind, &catalogue::RENAME) {
                    ops::rename_then_mark(&t, t.id("p/x"), to).await;
                } else {
                    ops::stage_respell(&t, t.id("p/x"), to).await;
                }
            })
        });
        answers(what, kind, tree(&["p/x"]), respell, done);
    }
    answers(
        "a dirty move of a directory",
        &catalogue::DIRTY_MOVE,
        tree(&["p/s/a", "p/s/t/x", "q/"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let (s, q) = (t.id("p/s"), t.id("q"));
                ops::dirty_move(&t, s, q, NameChange::Keep)
                    .await
                    .expect("'p/s' moves");
            })
        }),
        done,
    );
    answers(
        "a stage move of a directory renamed over the directory its name holds",
        &catalogue::STAGE_MOVE,
        tree(&["p/s/a", "p/s/t/x", "q/r/z"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let (s, q, r) = (t.id("p/s"), t.id("q"), t.id("q/r"));
                ops::stage_move(&t, s, q, NameChange::Rename("r"), Some(r))
                    .await
                    .expect("'p/s' moves");
            })
        }),
        done,
    );
    answers(
        "a stage move within its directory",
        &catalogue::STAGE_MOVE,
        tree(&["p/x"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let (x, p) = (t.id("p/x"), t.id("p"));
                ops::stage_move(&t, x, p, NameChange::Rename("y"), None)
                    .await
                    .expect("'p/x' moves");
            })
        }),
        done,
    );
    answers(
        "a realize move in a merge",
        &catalogue::REALIZE_MOVE,
        tree(&["p/x", "q/"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let (x, q) = (t.id("p/x"), t.id("q"));
                ops::realize_move(&t, x, q, NameChange::Keep, true)
                    .await
                    .expect("'p/x' moves");
            })
        }),
        done,
    );
    answers(
        "a drain of an empty queue",
        &catalogue::DRAIN,
        tree(&["d/"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let _ = ops::drain(&t).await;
            })
        }),
        |outcome| {
            *outcome
                == Outcome::Done(Answer::Freed {
                    files: 0,
                    directories: 0,
                })
        },
    );
    answers(
        "a find of a path",
        &catalogue::FIND_PATH,
        tree(&["d/e/"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { assert!(ops::find_dir(&t, "d/e").await.is_some()) })
        }),
        |outcome| matches!(outcome, Outcome::Answer(Answer::Found(Some(_)))),
    );
    answers(
        "a find of no path",
        &catalogue::FIND_PATH,
        tree(&["d/"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { assert!(ops::find_dir(&t, "d/e").await.is_none()) })
        }),
        |outcome| *outcome == Outcome::Answer(Answer::Found(None)),
    );
    answers(
        "a find of a name",
        &catalogue::FIND,
        tree(&["d/f"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { assert!(ops::find(&t, t.id("d"), "f").await.is_some()) })
        }),
        |outcome| matches!(outcome, Outcome::Answer(Answer::Found(Some(_)))),
    );
    answers(
        "a find of no name",
        &catalogue::FIND,
        tree(&["d/"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { assert!(ops::find(&t, t.id("d"), "f").await.is_none()) })
        }),
        |outcome| *outcome == Outcome::Answer(Answer::Found(None)),
    );
    answers(
        "a restore of a node staged for deletion",
        &catalogue::RESTORE,
        changed(&["d/f"], |t| {
            Box::pin(ops::staged_delete_level(t, t.id("d/f"), t.id("d")))
        }),
        Box::new(|t: Arc<Tree>| {
            Box::pin(
                async move { assert!(ops::restore(&t, t.id("d/f"), &Content::default()).await) },
            )
        }),
        done,
    );
    answers(
        "a modify of a file",
        &catalogue::STAGED_MODIFY,
        tree(&["d/f"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let content = Content {
                    size: 3,
                    ..Content::default()
                };
                assert!(ops::staged_modify(&t, t.id("d/f"), &content).await);
            })
        }),
        done,
    );
    answers(
        "a pend of a directory a delete queued already",
        &catalogue::PEND,
        Box::new(|| {
            Box::pin(async {
                let tree = staged_directory()().await;
                let place = (tree.id("s/t"), tree.id("s"), hash_string("t"));
                assert!(ops::pend(&tree, place, async {}).await);
                tree
            })
        }),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let place = (t.id("s/t"), t.id("s"), hash_string("t"));
                assert!(!ops::pend(&t, place, async {}).await);
            })
        }),
        refused(Refusal::NotFound),
    );
    answers(
        "a flip of a dirty file",
        &catalogue::FLIP,
        changed(&["d/f"], |t| {
            Box::pin(ops::scan_mark_modified(t, t.id("d/f")))
        }),
        Box::new(|t: Arc<Tree>| Box::pin(async move { assert!(ops::flip(&t, t.id("d/f")).await) })),
        done,
    );
    answers(
        "a flip of a file gone",
        &catalogue::FLIP,
        changed(&["d/f"], |t| {
            Box::pin(async move { assert!(ops::unserved_detach(t, t.id("d/f")).await) })
        }),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { assert!(!ops::flip(&t, t.id("d/f")).await) })
        }),
        refused(Refusal::NotFound),
    );
    answers(
        "an owned change of a file gone",
        &catalogue::OWNED,
        changed(&["d/f"], |t| {
            Box::pin(async move { assert!(ops::unserved_detach(t, t.id("d/f")).await) })
        }),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { assert!(!ops::owned_size(&t, t.id("d/f"), 3).await) })
        }),
        refused(Refusal::Declined),
    );
    answers(
        "a clean rewrite of a clean file",
        &catalogue::SWAP_REWRITE,
        tree(&["d/f"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { assert!(ops::swap_rewrite_if_clean(&t, t.id("d/f")).await) })
        }),
        done,
    );
    answers(
        "an add then discard of a file",
        &catalogue::ADD_DISCARD,
        tree(&["d/"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { ops::add_discard(&t, t.id("d"), "n").await })
        }),
        |outcome| {
            *outcome
                == Outcome::Done(Answer::Freed {
                    files: 1,
                    directories: 0,
                })
        },
    );
    answers(
        "an add then discard of a directory holding a file and a directory of a file",
        &catalogue::ADD_DISCARD_TREE,
        tree(&["d/"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { ops::add_discard_tree(&t, t.id("d"), "n").await })
        }),
        |outcome| {
            *outcome
                == Outcome::Done(Answer::Freed {
                    files: 2,
                    directories: 2,
                })
        },
    );
    answers(
        "a swap's rewrite of a clean file",
        &catalogue::SWAP_REWRITE,
        tree(&["d/f"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let content = Content {
                    size: 3,
                    ..Content::default()
                };
                assert!(ops::swap_rewrite(&t, t.id("d/f"), &content).await);
            })
        }),
        done,
    );
    answers(
        "a swap's rewrite of a dirty file",
        &catalogue::SWAP_REWRITE,
        changed(&["d/f"], |t| {
            Box::pin(ops::scan_mark_modified(t, t.id("d/f")))
        }),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let content = Content {
                    size: 3,
                    ..Content::default()
                };
                assert!(!ops::swap_rewrite(&t, t.id("d/f"), &content).await);
            })
        }),
        refused(Refusal::IfClean),
    );
    answers(
        "a save",
        &catalogue::SERIALIZE,
        tree(&["d/f"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let _ = t.save().await;
            })
        }),
        done,
    );
    answers(
        "a clean rewrite of a dirty file",
        &catalogue::SWAP_REWRITE,
        changed(&["d/f"], |t| {
            Box::pin(ops::scan_mark_modified(t, t.id("d/f")))
        }),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { assert!(!ops::swap_rewrite_if_clean(&t, t.id("d/f")).await) })
        }),
        refused(Refusal::IfClean),
    );
}

/// Each served op answers each outcome it can reach from a served State that decides it, alone.
#[test]
fn each_served_op_answers_what_it_found() {
    answers(
        "a served create",
        &catalogue::SERVED_CREATE,
        served(&["d/"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { assert!(ops::served_create(&t, t.id("d"), "n").await.is_some()) })
        }),
        |outcome| matches!(outcome, Outcome::Done(Answer::Node(_))),
    );
    answers(
        "a served create of a held name",
        &catalogue::SERVED_CREATE,
        served(&["d/f"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { assert!(ops::served_create(&t, t.id("d"), "f").await.is_none()) })
        }),
        refused(Refusal::Exists),
    );
    answers(
        "a listing create",
        &catalogue::LISTING_CREATE,
        served(&["d/f"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(
                async move { assert!(ops::listing_create(&t, t.id("d"), "n").await.is_some()) },
            )
        }),
        |outcome| matches!(outcome, Outcome::Done(Answer::Node(_))),
    );
    answers(
        "a listing create of a held name",
        &catalogue::LISTING_CREATE,
        served(&["d/f"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(
                async move { assert!(ops::listing_create(&t, t.id("d"), "f").await.is_none()) },
            )
        }),
        refused(Refusal::Exists),
    );
    answers(
        "a served rename of a file",
        &catalogue::SERVED_RENAME,
        served(&["d/f", "e/"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(
                async move { assert!(ops::served_rename(&t, t.id("d/f"), t.id("e"), "g").await) },
            )
        }),
        |outcome| *outcome == Outcome::Done(Answer::Moved { added: false }),
    );
    answers(
        "a served rename replacing the file holding its name",
        &catalogue::SERVED_RENAME_REPLACING,
        served(&["d/f", "d/g"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let renamed =
                    ops::served_rename_replacing(&t, t.id("d/f"), (t.id("d"), "g"), t.id("d/g"))
                        .await;
                assert!(renamed, "'d/f' replaces 'd/g'");
            })
        }),
        |outcome| matches!(outcome, Outcome::Done(Answer::Moved { .. })),
    );
    answers(
        "a served rename onto a held name",
        &catalogue::SERVED_RENAME,
        served(&["d/f", "d/g"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(
                async move { assert!(!ops::served_rename(&t, t.id("d/f"), t.id("d"), "g").await) },
            )
        }),
        refused(Refusal::Exists),
    );
    answers(
        "a served rename of a directory without local content",
        &catalogue::SERVED_RENAME,
        served(&["d/s/", "e/"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(
                async move { assert!(!ops::served_rename(&t, t.id("d/s"), t.id("e"), "s").await) },
            )
        }),
        refused(Refusal::NotLocal),
    );
    answers(
        "a listing rename of a file",
        &catalogue::LISTING_RENAME,
        served(&["d/f", "e/"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(
                async move { assert!(ops::listing_rename(&t, t.id("d/f"), t.id("e"), "g").await) },
            )
        }),
        |outcome| *outcome == Outcome::Done(Answer::Moved { added: false }),
    );
    answers(
        "a listing rename onto a held name",
        &catalogue::LISTING_RENAME,
        served(&["d/f", "d/g"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(
                async move { assert!(!ops::listing_rename(&t, t.id("d/f"), t.id("d"), "g").await) },
            )
        }),
        refused(Refusal::Exists),
    );
    answers(
        "a listing rename of a directory without local content",
        &catalogue::LISTING_RENAME,
        served(&["d/s/", "e/"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(
                async move { assert!(!ops::listing_rename(&t, t.id("d/s"), t.id("e"), "s").await) },
            )
        }),
        refused(Refusal::NotLocal),
    );
    answers(
        "a served detach of a node gone",
        &catalogue::SERVED_DETACH,
        Box::new(|| {
            Box::pin(async {
                let tree = Tree::new(&["d/f"]).await;
                tree.state.mark_served();
                assert!(ops::served_detach(&tree, tree.id("d/f")).await);
                tree
            })
        }),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { assert!(!ops::served_detach(&t, t.id("d/f")).await) })
        }),
        refused(Refusal::NotFound),
    );
    answers(
        "a served RMDIR of a directory holding a child",
        &catalogue::SERVED_RMDIR,
        served(&["d/f"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { assert!(!ops::served_rmdir(&t, t.id("d")).await.removed) })
        }),
        refused(Refusal::NotEmpty),
    );
    answers(
        "a served RMDIR of an empty directory",
        &catalogue::SERVED_RMDIR,
        served(&["d/e/"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { assert!(ops::served_rmdir(&t, t.id("d/e")).await.removed) })
        }),
        |outcome| *outcome == Outcome::Done(Answer::None),
    );
    answers(
        "a settle of a clean directory",
        &catalogue::SETTLE,
        served(&["d/f"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                assert_eq!(ops::settle(&t, t.id("d")).await, ops::Settle::NotBegun);
            })
        }),
        refused(Refusal::NotBegun),
    );
    answers(
        "a swap's copy onto a held name",
        &catalogue::SWAP_ATTACH,
        served(&["d/f"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { assert!(!ops::swap_attach(&t, t.id("d"), "f").await) })
        }),
        refused(Refusal::Exists),
    );
    answers(
        "a swap's clean detach of a dirty directory",
        &catalogue::SWAP_DETACH_IF_CLEAN,
        Box::new(|| {
            Box::pin(async {
                let tree = Tree::new(&["d/e/"]).await;
                tree.state.mark_served();
                assert!(ops::served_mark_dir(&tree, tree.id("d/e")).await);
                tree
            })
        }),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { assert!(!ops::swap_detach_if_clean(&t, t.id("d/e")).await) })
        }),
        refused(Refusal::IfClean),
    );
    answers(
        "a served detach of a file",
        &catalogue::SERVED_DETACH,
        served(&["d/f"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { assert!(ops::served_detach(&t, t.id("d/f")).await) })
        }),
        done,
    );
    answers(
        "a served mark of a directory gone",
        &catalogue::SERVED_MARK,
        Box::new(|| {
            Box::pin(async {
                let tree = Tree::new(&["d/e/"]).await;
                tree.state.mark_served();
                assert!(ops::served_detach(&tree, tree.id("d/e")).await);
                tree
            })
        }),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { assert!(!ops::served_mark_dir(&t, t.id("d/e")).await) })
        }),
        refused(Refusal::NotFound),
    );
    answers(
        "a served copy-up of a file",
        &catalogue::SERVED_COPY_UP,
        served(&["d/f"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { assert!(ops::served_copy_up(&t, t.id("d/f"), t.id("d")).await) })
        }),
        done,
    );
    answers(
        "a served copy-up of a file gone",
        &catalogue::SERVED_COPY_UP,
        Box::new(|| {
            Box::pin(async {
                let tree = Tree::new(&["d/f"]).await;
                tree.state.mark_served();
                assert!(ops::served_detach(&tree, tree.id("d/f")).await);
                tree
            })
        }),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { assert!(!ops::served_copy_up(&t, t.id("d/f"), t.id("d")).await) })
        }),
        refused(Refusal::NotFound),
    );
    answers(
        "a settle of a dirty directory holding what its commit holds",
        &catalogue::SETTLE,
        marked(&["d/f"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                assert_eq!(ops::settle(&t, t.id("d")).await, ops::Settle::Cleared);
            })
        }),
        done,
    );
    answers(
        "a settle of a directory holding a child its commit does not",
        &catalogue::SETTLE,
        Box::new(|| {
            Box::pin(async {
                let tree = Tree::new(&["d/f"]).await;
                tree.state.mark_served();
                assert!(ops::served_create(&tree, tree.id("d"), "n").await.is_some());
                tree
            })
        }),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                assert_eq!(ops::settle(&t, t.id("d")).await, ops::Settle::KeptUnequal);
            })
        }),
        refused(Refusal::Unequal),
    );
    // A served create in the settle's window, its call apart from the settle's: its mark drops
    // the settle's pending clear, which the settle answers as overtaken.
    explore(
        marked(&["d/f"]),
        vec![Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let created = async {
                    let created = ops::served_create(&t, t.id("d"), "y").await;
                    assert!(created.is_some(), "'d/y' is created");
                };
                let settled = ops::settle_waiting(&t, t.id("d"), created).await;
                assert_eq!(settled, ops::Settle::KeptAtFinish);
            })
        })],
        vec![],
        |run| {
            assert_eq!(
                run.calls.len(),
                2,
                "the settle's and the create's: {:#?}",
                run.calls
            );
            let settled = call_of(run, &catalogue::SETTLE);
            assert_eq!(
                settled.outcome,
                Outcome::Refused(Refusal::Overtaken),
                "a settle a create in its window overtakes"
            );
            let created = call_of(run, &catalogue::SERVED_CREATE);
            assert!(
                matches!(created.outcome, Outcome::Done(Answer::Node(_))),
                "the create answers its node: {:?}",
                created.outcome
            );
        },
    );
    answers(
        "a settle whose directory moves in its window",
        &catalogue::SETTLE,
        marked(&["d/f", "e/"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let moved = async {
                    let moved = ops::served_reattach_any(&t, t.id("d"), t.id("e"), "d").await;
                    assert_eq!(moved, Some(true), "'d' moves");
                };
                let settled = ops::settle_waiting(&t, t.id("d"), moved).await;
                assert_eq!(settled, ops::Settle::KeptAtFinish);
            })
        }),
        refused(Refusal::Overtaken),
    );
    answers(
        "a served clear of a dirty directory holding what its commit holds",
        &catalogue::SETTLE,
        marked(&["d/f"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let cleared = ops::served_clear(&t, t.id("d")).await;
                assert!(matches!(cleared, Ok(ClearOutcome::Cleared)), "{cleared:?}");
            })
        }),
        done,
    );
    answers(
        "a served clear of a directory holding a child its commit does not",
        &catalogue::SETTLE,
        Box::new(|| {
            Box::pin(async {
                let tree = Tree::new(&["d/f"]).await;
                tree.state.mark_served();
                assert!(ops::served_create(&tree, tree.id("d"), "n").await.is_some());
                tree
            })
        }),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let cleared = ops::served_clear(&t, t.id("d")).await;
                assert!(matches!(cleared, Ok(ClearOutcome::Kept)), "{cleared:?}");
            })
        }),
        refused(Refusal::Unequal),
    );
    answers(
        "a served clear of a directory moved from its built place",
        &catalogue::SETTLE,
        Box::new(|| {
            Box::pin(async {
                let tree = Tree::new(&["d/f", "e/"]).await;
                tree.state.mark_served();
                let moved = ops::served_reattach_any(&tree, tree.id("d"), tree.id("e"), "d").await;
                assert_eq!(moved, Some(true));
                tree
            })
        }),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                let cleared = ops::served_clear(&t, t.id("d")).await;
                assert!(matches!(cleared, Ok(ClearOutcome::Moved)), "{cleared:?}");
            })
        }),
        refused(Refusal::NotBegun),
    );
    answers(
        "a swap's copy of a file",
        &catalogue::SWAP_ATTACH,
        served(&["d/"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { assert!(ops::swap_attach(&t, t.id("d"), "f").await) })
        }),
        |outcome| matches!(outcome, Outcome::Done(Answer::Node(_))),
    );
    answers(
        "a swap's clean detach of a clean directory",
        &catalogue::SWAP_DETACH_IF_CLEAN,
        served(&["d/e/"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { assert!(ops::swap_detach_if_clean(&t, t.id("d/e")).await) })
        }),
        done,
    );
    answers(
        "a swap's clean rewrite of a clean file",
        &catalogue::SWAP_REWRITE,
        served(&["d/f"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move { assert!(ops::swap_rewrite_if_clean(&t, t.id("d/f")).await) })
        }),
        done,
    );
    answers(
        "a swap's copy",
        &catalogue::SWAP_COPY_TREE,
        served(&["d/"]),
        Box::new(|t: Arc<Tree>| {
            Box::pin(async move {
                assert!(
                    ops::swap_copy_tree(&t, t.id("d"), "c", &["i"])
                        .await
                        .is_some()
                );
            })
        }),
        |outcome| matches!(outcome, Outcome::Done(Answer::Node(_))),
    );
}

/// The call of `kind` among `run`'s calls.
fn call_of<'r>(run: &'r Run<'_>, kind: &'static Kind) -> &'r Call {
    run.calls
        .iter()
        .find(|call| std::ptr::eq(call.kind, kind))
        .expect("the call of the kind")
}

/// What a find's call found.
fn found(call: &Call) -> Option<NodeID> {
    let Outcome::Answer(Answer::Found(found)) = call.outcome else {
        panic!("a find answered {:?}", call.outcome);
    };
    found
}

/// What a find made in one run beside a change.
struct FindRun {
    found: Option<NodeID>,
    /// The tree's path of the node it found.
    path: Option<String>,
    /// Whether it walked its chain again: a `FindRestart` of its own.
    walked_again: bool,
    /// Its call's begin and end, and the `seq` of the run's first unlink and first move, where it
    /// made one.
    begin: u64,
    end: u64,
    unlinked: Option<u64>,
    moved: Option<u64>,
}

/// Explores `find`, a find of `kind`'s, beside `change` on the tree `setup` builds, the find's walk
/// parking at each step: each lookup replays clean over its call. What the find made in each run.
fn finds_beside(
    setup: Setup<Tree>,
    (find, kind): (OpFn, &'static Kind),
    change: OpFn,
) -> Vec<FindRun> {
    explored_within(None, setup, (find, kind), vec![change], |run, call| {
        let walked_again = run.records.iter().any(|record| {
            matches!(record.effect, Effect::Arm(Arm::FindRestart))
                && call.holds(record, &run.spawns)
        });
        let first = |effect: fn(&Effect) -> bool| {
            (run.records.iter())
                .find(|record| effect(&record.effect))
                .map(|record| record.seq)
        };
        let unlinked = first(|effect| matches!(effect, Effect::Detach { .. }));
        let moved = first(|effect| matches!(effect, Effect::Move { .. }));
        let found = found(call);
        let path = run
            .tree
            .nodes
            .iter()
            .find(|&&(_, node)| Some(node) == found)
            .map(|(path, _)| path.clone());
        FindRun {
            found,
            path,
            walked_again,
            begin: call.begin,
            end: call.end,
            unlinked,
            moved,
        }
    })
}

/// The find of `d/f` the explorations make beside a change.
fn find_d_f() -> (OpFn, &'static Kind) {
    (
        |tree| {
            Box::pin(async move {
                let _ = ops::find(&tree, tree.id("d"), "f").await;
            })
        },
        &catalogue::FIND,
    )
}

/// How many of `runs` found a node, and how many missed.
fn hits_and_misses(runs: &[FindRun]) -> (usize, usize) {
    let hits = runs.iter().filter(|run| run.found.is_some()).count();
    (hits, runs.len() - hits)
}

/// A find of `d/f` beside a change of its name: a detach, a dirty move renaming it within `d`, a
/// move out to `e`, a create where none stood. The find answers the node in some runs and misses
/// in others.
#[test]
fn a_find_beside_a_change_of_its_name_answers_what_the_model_holds() {
    let changes: [(&'static [&'static str], OpFn); 4] = [
        (&["d/f"], |tree| {
            Box::pin(async move { assert!(ops::unserved_detach(&tree, tree.id("d/f")).await) })
        }),
        (&["d/f"], |tree| {
            Box::pin(async move { renamed_within(&tree, "d/f", "g").await })
        }),
        (&["d/f", "e/"], |tree| {
            Box::pin(async move {
                ops::dirty_move(&tree, tree.id("d/f"), tree.id("e"), NameChange::Keep)
                    .await
                    .expect("'d/f' moves");
            })
        }),
        (&["d/"], |tree| {
            Box::pin(async move {
                assert!(
                    ops::checked_create(&tree, tree.id("d"), "f")
                        .await
                        .is_some()
                );
            })
        }),
    ];
    for (paths, change) in changes {
        let (hits, misses) = hits_and_misses(&finds_beside(tree(paths), find_d_f(), change));
        assert!(
            hits > 0 && misses > 0,
            "{paths:?}: found {hits}, missed {misses}"
        );
    }
}

/// A find of `d/f` beside a move of `d/x`, the chain's head, into `e`, which holds a node named
/// `f` too: a walk standing on `x` as it moves finds its parent changed and walks again from the
/// head (`ChainWalk::step`), never following `x` into `e`. `d/f` carries its name throughout, so
/// the find answers it in every run, and in some it walked again.
#[test]
fn a_find_beside_a_move_of_the_node_it_stands_on_walks_again() {
    let runs = finds_beside(tree(&["d/f", "d/x", "e/f"]), find_d_f(), |tree| {
        Box::pin(async move {
            ops::dirty_move(&tree, tree.id("d/x"), tree.id("e"), NameChange::Keep)
                .await
                .expect("'d/x' moves");
        })
    });
    assert!(
        runs.iter().all(|run| run.path.as_deref() == Some("d/f")),
        "a find missed 'd/f' or answered another node"
    );
    assert!(
        runs.iter().any(|run| run.walked_again),
        "no find walked again"
    );
}

/// A find of `d/r` beside a stage's move of `p/s` into `d` renamed `r`, over the directory `d/r`
/// holds: the holder where the find ended before the move, and the moved directory, prepended ahead
/// of it, where the find began after it; either where they overlap; each in some runs. The holder
/// stays, staged for deletion, under the name until the commit.
#[test]
fn a_find_beside_a_stage_move_over_a_held_name_answers_either_node() {
    let find: OpFn = |tree| {
        Box::pin(async move {
            let _ = ops::find(&tree, tree.id("d"), "r").await;
        })
    };
    let runs = finds_beside(
        tree(&["p/s/a", "d/r/z"]),
        (find, &catalogue::FIND),
        |tree| {
            Box::pin(async move {
                let (s, d, r) = (tree.id("p/s"), tree.id("d"), tree.id("d/r"));
                (ops::stage_move(&tree, s, d, NameChange::Rename("r"), Some(r)).await)
                    .expect("'p/s' moves");
            })
        },
    );
    for run in &runs {
        let moved = run.moved.expect("the stage's move moved 'p/s'");
        let path = run.path.as_deref();
        if moved < run.begin {
            assert_eq!(path, Some("p/s"), "a find after the move");
        }
        if run.end < moved {
            assert_eq!(path, Some("d/r"), "a find before the move");
        }
    }
    let found: Vec<Option<&str>> = runs.iter().map(|run| run.path.as_deref()).collect();
    for path in ["d/r", "p/s"] {
        assert!(
            found.contains(&Some(path)),
            "no find answered '{path}': {found:?}"
        );
    }
    assert!(
        (found.iter()).all(|path| matches!(path, Some("d/r" | "p/s"))),
        "{found:?}"
    );
}

/// A find of `s/t/f` beside a C API delete of `s/t`, which marks its subtree `Discarded` in place
/// with no change to the count: the find answers the node in some runs, and misses it in others
/// once the mark skips it.
#[test]
fn a_find_beside_a_pending_discard_of_its_directory_skips_what_it_marked() {
    let setup: Setup<Tree> = Box::new(|| {
        Box::pin(async {
            let mut tree = staged_directory()().await;
            let f = ops::staged_add(&tree, tree.id("s/t"), file("f"), "f")
                .await
                .expect("'s/t/f' adds");
            tree.nodes.push(("s/t/f".to_string(), f));
            tree
        })
    });
    let find: OpFn = |tree| {
        Box::pin(async move {
            let _ = ops::find(&tree, tree.id("s/t"), "f").await;
        })
    };
    let runs = finds_beside(setup, (find, &catalogue::FIND), |tree| {
        Box::pin(async move {
            let place = (tree.id("s/t"), tree.id("s"), hash_string("t"));
            assert!(ops::pend(&tree, place, async {}).await);
        })
    });
    let (hits, misses) = hits_and_misses(&runs);
    assert!(hits > 0 && misses > 0, "found {hits}, missed {misses}");
}

/// A find of `p/d` in `p` beside a detach of `p`: the find walks the detached directory's chain,
/// which still holds `d`, so it answers `d` in every run, some begun after the unlink.
#[test]
fn a_find_in_a_detached_directory_walks_its_chain() {
    let find: OpFn = |tree| {
        Box::pin(async move {
            let _ = ops::find(&tree, tree.id("p"), "d").await;
        })
    };
    let runs = finds_beside(tree(&["p/d/"]), (find, &catalogue::FIND), |tree| {
        Box::pin(async move { assert!(ops::unserved_detach(&tree, tree.id("p")).await) })
    });
    assert_eq!(hits_and_misses(&runs).1, 0, "a find missed 'p/d'");
    assert!(
        runs.iter()
            .any(|run| run.unlinked.is_some_and(|unlinked| run.begin > unlinked)),
        "no find began after the unlink"
    );
}

/// A path find beside a detach of the directory it ends at, and beside a detach of the directory
/// it walks through: each hop replays clean as a find, the last found in some runs and missed in
/// others.
#[test]
fn a_path_find_beside_a_detach_answers_per_hop() {
    let detaches: [OpFn; 2] = [
        |tree| Box::pin(async move { assert!(ops::unserved_detach(&tree, tree.id("p/d")).await) }),
        |tree| Box::pin(async move { assert!(ops::unserved_detach(&tree, tree.id("p")).await) }),
    ];
    let find: OpFn = |tree| {
        Box::pin(async move {
            let _ = ops::find_dir(&tree, "p/d").await;
        })
    };
    for detach in detaches {
        let runs = finds_beside(tree(&["p/d/"]), (find, &catalogue::FIND_PATH), detach);
        let (hits, misses) = hits_and_misses(&runs);
        assert!(hits > 0 && misses > 0, "found {hits}, missed {misses}");
    }
}

/// A find of `d/f` beside a served rename of `d/g` onto it, replacing `d/f` (C7): the find answers
/// either node, the one replaced in some runs and the one replacing it in others, and never misses
/// the name, which a node carries throughout.
#[test]
fn a_find_beside_a_replacing_rename_answers_either_node() {
    let setup = served(&["d/f", "d/g"]);
    let runs = finds_beside(setup, find_d_f(), |tree| {
        Box::pin(async move {
            let (d, f, g) = (tree.id("d"), tree.id("d/f"), tree.id("d/g"));
            assert!(ops::served_rename_replacing(&tree, g, (d, "f"), f).await);
        })
    });
    let answered = |path: &str| {
        runs.iter()
            .filter(|run| run.path.as_deref() == Some(path))
            .count()
    };
    let (replaced, replacing) = (answered("d/f"), answered("d/g"));
    assert_eq!(
        replaced + replacing,
        runs.len(),
        "a find missed or answered another node"
    );
    assert!(
        replaced > 0 && replacing > 0,
        "found the replaced {replaced}, the replacing {replacing}"
    );
}

/// A find of `d/f` beside a served respell of `d/f`, which keeps its name's hash and moves it to
/// the chain's head: a walk that has passed the head reaches the end without the name and, the
/// count moved, walks again (`ChainWalk::missed_a_reattach`). `d/f` carries its name throughout,
/// so the find answers it in every run, and in some it walked again.
#[test]
fn a_find_beside_a_respell_of_its_name_walks_again_and_finds_it() {
    let runs = finds_beside(served(&["d/f", "d/g"]), find_d_f(), |tree| {
        Box::pin(async move {
            let (d, f) = (tree.id("d"), tree.id("d/f"));
            assert!(ops::served_rename(&tree, f, d, "F").await);
        })
    });
    assert_eq!(hits_and_misses(&runs).1, 0, "a find missed 'd/f'");
    assert!(
        runs.iter().any(|run| run.walked_again),
        "no find walked again"
    );
}

/// The worker's resident find of `d/f` beside its served detach, on a served State with its
/// blocks in memory: each lookup replays clean, the node found in some runs and missed in others.
/// It has no scheduling point, so it runs whole between the detach's steps.
#[test]
fn a_resident_find_beside_a_detach_answers_what_the_model_holds() {
    let find: OpFn = |tree| {
        Box::pin(async move {
            let _ = ops::find_resident(&tree, tree.id("d"), "f").await;
        })
    };
    let runs = finds_beside(
        served(&["d/f"]),
        (find, &catalogue::FIND_RESIDENT),
        |tree| Box::pin(async move { assert!(ops::served_detach(&tree, tree.id("d/f")).await) }),
    );
    let (hits, misses) = hits_and_misses(&runs);
    assert!(hits > 0 && misses > 0, "found {hits}, missed {misses}");
}

/// The worker's resident find with its directory's block out of memory self-reports `NeedLoad`,
/// looking nothing up, in a run that admits it.
#[test]
fn a_resident_find_with_its_block_out_of_memory_needs_a_load() {
    let setup: Setup<Tree> = Box::new(|| {
        Box::pin(async {
            let tree = Tree::new(&["d/f"]).await.reloaded().await;
            tree.state.mark_served();
            tree
        })
    });
    explore_with(
        Explore {
            self_reports: &[SelfReported::NeedLoad],
            ..Explore::default()
        },
        setup,
        vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                tree.state.evict_blocks_for_tests();
                assert!(ops::find_resident(&tree, tree.id("d"), "f").await.is_none());
            })
        })],
        vec![],
        |run| {
            let find = call_of(run, &catalogue::FIND_RESIDENT);
            assert_eq!(find.outcome, Outcome::SelfReported(SelfReported::NeedLoad));
            assert!(find.lookups.is_empty(), "{:?}", find.lookups);
        },
    );
}

/// The worker's resident find with its directory's block in memory and the block its chain begins
/// in out of it self-reports `NeedLoad` from its walk, in a run that admits it.
#[test]
fn a_resident_find_whose_walk_meets_a_block_out_of_memory_needs_a_load() {
    let setup: Setup<Tree> = Box::new(|| {
        Box::pin(async {
            let names: Vec<String> = (0..BLOCK_NODE_COUNT + 8)
                .map(|index| format!("d/f{index}"))
                .collect();
            let paths: Vec<&str> = names.iter().map(String::as_str).collect();
            let tree = Tree::new(&paths).await.reloaded().await;
            tree.state.mark_served();
            // The directory's block loaded, outside every call.
            let _ = tree.state.node(tree.repository.clone(), tree.id("d")).await;
            tree
        })
    });
    explore_with(
        Explore {
            self_reports: &[SelfReported::NeedLoad],
            ..Explore::default()
        },
        setup,
        vec![Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let d = tree.id("d");
                // Held, so the directory's block stays in memory.
                let block = tree
                    .state
                    .resident_block(NodeBlock::index(d))
                    .expect("the directory's block in memory");
                let head = block.node(Node::index(d)).child().expect("a child");
                assert_ne!(
                    NodeBlock::index(head),
                    NodeBlock::index(d),
                    "the chain begins in another block than the directory's"
                );
                tree.state.evict_blocks_for_tests();
                assert!(
                    tree.state.resident_block(NodeBlock::index(head)).is_none(),
                    "the chain's first block is out of memory"
                );
                assert!(ops::find_resident(&tree, d, "f0").await.is_none());
                drop(block);
            })
        })],
        vec![],
        |run| {
            let find = call_of(run, &catalogue::FIND_RESIDENT);
            assert_eq!(find.outcome, Outcome::SelfReported(SelfReported::NeedLoad));
            assert!(find.lookups.is_empty(), "{:?}", find.lookups);
        },
    );
}

/// What a walk made in one run of an exploration: the tree's paths of the nodes it yielded, in
/// order, a node the tree did not build by its id; what it reported of its repeats; the restarts
/// its call armed as `walk_children` does; the `seq` of its head read and its call's end; each node
/// other tasks unlinked or marked `Discarded`, each they moved or renamed, each they published, and
/// each they wrote words of, by path, with the `seq` they did and the words.
struct WalkRun {
    yielded: Vec<String>,
    repeats: Repeats,
    restarts: usize,
    head: u64,
    end: u64,
    gone: Vec<(String, u64)>,
    moved: Vec<(String, u64)>,
    published: Vec<(String, u64)>,
    words: Vec<(String, u64, Node)>,
}

impl WalkRun {
    /// Whether another task published a node after the walk's head read, within its call, that the
    /// walk did not yield.
    fn published_unyielded(&self) -> bool {
        (self.published.iter())
            .any(|(path, seq)| self.head < *seq && *seq < self.end && !self.yields(path))
    }

    /// Whether it yielded the node at `path`.
    fn yields(&self, path: &str) -> bool {
        self.yielded.iter().any(|yielded| yielded == path)
    }

    /// The `seq` another task first took the node at `path` out of the walk's sight at.
    fn gone_at(&self, path: &str) -> Option<u64> {
        (self.gone.iter())
            .find(|(gone, _)| gone == path)
            .map(|&(_, seq)| seq)
    }

    /// Whether another task took the node at `path` out of the walk's sight after its head read,
    /// within its call.
    fn gone_while_walking(&self, path: &str) -> bool {
        self.gone_at(path)
            .is_some_and(|seq| self.head < seq && seq < self.end)
    }

    /// Whether another task moved or renamed the node at `path` after the walk's head read, within
    /// its call.
    fn moved_while_walking(&self, path: &str) -> bool {
        (self.moved.iter()).any(|(moved, seq)| moved == path && self.head < *seq && *seq < self.end)
    }

    /// Whether another task wrote the node at `path` words `test` holds of after the walk's head
    /// read, within its call.
    fn worded_while_walking(&self, path: &str, test: fn(&Node) -> bool) -> bool {
        (self.words.iter()).any(|(worded, seq, words)| {
            worded == path && self.head < *seq && *seq < self.end && test(words)
        })
    }

    /// Whether another task wrote the node at `path` words `test` holds of before the walk's head
    /// read.
    fn worded_before_walking(&self, path: &str, test: fn(&Node) -> bool) -> bool {
        (self.words.iter())
            .any(|(worded, seq, words)| worded == path && *seq < self.head && test(words))
    }
}

/// Explores `walk`, a walk of `kind`'s, beside `changes` on the tree `setup` builds, the walk
/// parking at each step: each walk replays clean over its call. What the walk made in each run.
fn walks_beside(
    setup: Setup<Tree>,
    (walk, kind): (OpFn, &'static Kind),
    changes: Vec<OpFn>,
) -> Vec<WalkRun> {
    walks_within(None, setup, (walk, kind), changes)
}

/// [`walks_beside`] within `preemptions` per run, where given.
fn walks_within(
    preemptions: Option<usize>,
    setup: Setup<Tree>,
    (walk, kind): (OpFn, &'static Kind),
    changes: Vec<OpFn>,
) -> Vec<WalkRun> {
    explored_within(preemptions, setup, (walk, kind), changes, |run, call| {
        let walk = call.walks.first().expect("the walk reached its end");
        let path = |node: NodeID| path_of(run.tree, node);
        let head = (call.head_read(walk, &run.records, &run.spawns))
            .expect("the walk read its directory's head");
        let restarts = (run.records.iter())
            .filter(|record| {
                matches!(record.effect, Effect::Arm(Arm::SearchRestart))
                    && call.holds(record, &run.spawns)
            })
            .count();
        let mut gone = Vec::new();
        let mut moved = Vec::new();
        let mut published = Vec::new();
        let mut words = Vec::new();
        for record in run.records.iter().filter(|record| record.task != call.task) {
            let seq = record.seq;
            match &record.effect {
                Effect::Detach { node, .. } => gone.push((path(*node), seq)),
                Effect::Move { node, .. } | Effect::Rename { node, .. } => {
                    moved.push((path(*node), seq));
                }
                Effect::Publish { node, .. } => published.push((path(*node), seq)),
                Effect::Word {
                    node,
                    record: written,
                    ..
                } => {
                    if written.0.is_discarded() {
                        gone.push((path(*node), seq));
                    }
                    words.push((path(*node), seq, written.0));
                }
                _ => {}
            }
        }
        WalkRun {
            yielded: walk
                .yielded
                .iter()
                .map(|yielded| path(yielded.node))
                .collect(),
            repeats: walk.repeats,
            restarts,
            head,
            end: call.end,
            gone,
            moved,
            published,
            words,
        }
    })
}

/// Explores `op`, of `kind`'s, beside `changes` on the tree `setup` builds, readers parking at
/// their own points, within `preemptions` per run where given: each run replays clean over its
/// calls. What `read` takes of each run and `op`'s call there, one run at least.
fn explored_within<T: 'static>(
    preemptions: Option<usize>,
    setup: Setup<Tree>,
    (op, kind): (OpFn, &'static Kind),
    changes: Vec<OpFn>,
    read: impl Fn(&Run<'_>, &Call) -> T + 'static,
) -> Vec<T> {
    let runs = Rc::new(RefCell::new(Vec::new()));
    let kept = runs.clone();
    let mut ops: Vec<Op<Tree>> = vec![Box::new(op)];
    ops.extend(
        changes
            .into_iter()
            .map(|change| Box::new(change) as Op<Tree>),
    );
    explore_with(
        Explore {
            reader_points: true,
            preemptions,
            ..Explore::default()
        },
        setup,
        ops,
        vec![],
        move |run| kept.borrow_mut().push(read(run, call_of(run, kind))),
    );
    runs.take()
}

/// The answers `read` made beside `changes` on the tree `setup` builds, within `preemptions` per
/// run where given, in each run.
fn answers_beside(
    preemptions: Option<usize>,
    setup: Setup<Tree>,
    (read, kind): (OpFn, &'static Kind),
    changes: Vec<OpFn>,
) -> Vec<Outcome> {
    explored_within(
        preemptions,
        setup,
        (read, kind),
        changes,
        |_, call: &Call| call.outcome.clone(),
    )
}

/// Each children walk of `d/` the explorations make, with its kind: the iterators', which report
/// whether they may have repeated, and the collections' through one, which drop their repeats
/// unreported and keep every child the iterator yields in these trees.
fn walks_of_d() -> [(OpFn, &'static Kind); 5] {
    [
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::children(&tree, tree.id("d")).await;
                })
            },
            &catalogue::CHILDREN,
        ),
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::children_from_parent(&tree, tree.id("d")).await;
                })
            },
            &catalogue::CHILDREN_FROM_PARENT,
        ),
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::named_children(&tree, tree.id("d")).await;
                })
            },
            &catalogue::NAMED_CHILDREN,
        ),
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::collected_children(&tree, tree.id("d")).await;
                })
            },
            &catalogue::COLLECTED_CHILDREN,
        ),
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::collected_named_children(&tree, tree.id("d")).await;
                })
            },
            &catalogue::COLLECTED_NAMED_CHILDREN,
        ),
    ]
}

/// Each children walk of `d/` beside a create in `d`: each replays clean over its call, the
/// created node yielded in some runs and not in others, in some published after the walk's head
/// read and not yielded; and some iterators report they may have repeated, the count moved since
/// their head read. The model judges each walk against the head and count it read together, so a
/// walk taking its head from an earlier record and its count fresh fails here (H10).
#[test]
fn a_walk_beside_a_create_in_its_directory_yields_what_the_model_holds() {
    for (walk, kind) in walks_of_d() {
        let runs = walks_beside(
            tree(&["d/f", "d/g"]),
            (walk, kind),
            vec![|tree| {
                Box::pin(async move {
                    assert!(
                        ops::checked_create(&tree, tree.id("d"), "n")
                            .await
                            .is_some()
                    );
                })
            }],
        );
        // The created node, which the tree did not build, by its id.
        let unbuilt = |run: &&WalkRun| run.yielded.iter().any(|path| !path.starts_with("d/"));
        let with = runs.iter().filter(unbuilt).count();
        assert!(
            with > 0 && with < runs.len(),
            "{}: the created node yielded in {with} of {} runs",
            kind.name,
            runs.len()
        );
        assert!(
            runs.iter().any(WalkRun::published_unyielded),
            "{}: no create landed past a walk",
            kind.name
        );
        assert!(some_may_repeat(&runs), "{}", kind.name);
    }
}

/// Whether the walks of `runs` report whether they may have repeated, as an iterator does, not
/// drop their repeats unreported, as a listing does.
fn report_repeats(runs: &[WalkRun]) -> bool {
    runs.iter().all(|run| run.repeats != Repeats::Dropped)
}

/// Whether some walk of `runs` reported it may have repeated, where they report it.
fn some_may_repeat(runs: &[WalkRun]) -> bool {
    !report_repeats(runs) || runs.iter().any(|run| run.repeats == Repeats::May)
}

/// Each children walk of `d/` beside a detach of `d/f`: each replays clean over its call, `d/f`
/// yielded in some runs and not in others, some detached while the walk was under way, `d/g` in
/// every run.
#[test]
fn a_walk_beside_a_detach_in_its_directory_yields_what_the_model_holds() {
    for (walk, kind) in walks_of_d() {
        let runs = walks_beside(
            tree(&["d/f", "d/g"]),
            (walk, kind),
            vec![|tree| {
                Box::pin(async move { assert!(ops::unserved_detach(&tree, tree.id("d/f")).await) })
            }],
        );
        let with = runs.iter().filter(|run| run.yields("d/f")).count();
        assert!(
            with > 0 && with < runs.len(),
            "{}: 'd/f' yielded in {with} of {} runs",
            kind.name,
            runs.len()
        );
        assert!(
            runs.iter().any(|run| run.gone_while_walking("d/f")),
            "{}: no detach while a walk was under way",
            kind.name
        );
        assert!(runs.iter().all(|run| run.yields("d/g")), "{}", kind.name);
    }
}

/// Each children walk of `d/` beside a dirty move renaming `d/f` within `d`, in place, which keeps
/// its position and counts on the chain: `d/f` yielded in every run, with the words and the name
/// of one name or the other, and some iterators report they may have repeated.
#[test]
fn a_walk_beside_a_rename_in_place_yields_the_node_in_every_run() {
    for (walk, kind) in walks_of_d() {
        let runs = walks_beside(
            tree(&["d/f", "d/g"]),
            (walk, kind),
            vec![|tree| Box::pin(async move { renamed_within(&tree, "d/f", "h").await })],
        );
        assert!(runs.iter().all(|run| run.yields("d/f")), "{}", kind.name);
        assert!(some_may_repeat(&runs), "{}", kind.name);
    }
}

/// Each children walk of `d/` beside a move of `d/x` into `e`, by each of the trusted moves: a walk
/// standing on `x` as it moves finds its parent changed and starts again past the last node it
/// visited (`ChainWalk::step`), reporting it may have repeated, and yields `d/f`, in `d`
/// throughout, in every run; never `e/f`, which a walk following `x` past its move would wander
/// into. Every schedule of each move: the stage's and the realize move's, which make the same
/// `node_move` and mark nothing the walk reads, within the seven preemptions that reach them all.
#[test]
fn a_walk_beside_a_move_of_the_node_it_stands_on_starts_again() {
    let moves: [(OpFn, &'static Kind); 3] = moves_into_e!("d/x");
    for (walk, kind) in walks_of_d() {
        for (moved, by) in moves {
            let bound = (!std::ptr::eq(by, &catalogue::DIRTY_MOVE)).then_some(7);
            let runs = walks_within(
                bound,
                tree(&["d/f", "d/x", "e/f"]),
                (walk, kind),
                vec![moved],
            );
            assert!(
                runs.iter()
                    .all(|run| run.yields("d/f") && !run.yields("e/f")),
                "{} beside a {}",
                kind.name,
                by.name
            );
            assert!(
                runs.iter().any(|run| {
                    run.repeats.may_pass_over()
                        && run.moved_while_walking("d/x")
                        && !run.yields("d/x")
                }),
                "{} beside a {}: no walk passed 'd/x' over as it moved",
                kind.name,
                by.name
            );
        }
    }
}

/// Each children walk of a `d/` whose chain crosses blocks beside each trusted move of `d` itself
/// into `e`: every child yielded in every run, as moving `d` leaves its chain as it is; beside the
/// dirty and the stage move, which mark each node below `d` moved as they go, in some a child's
/// mark landing while the walk was under way. Within three preemptions a run, which land the marks
/// inside a walk.
#[test]
fn a_walk_of_a_directory_beside_its_move_yields_each_child_as_its_mark_lands() {
    let children = ["d/f", "d/g", "d/x", "d/y"];
    for (walk, kind) in walks_of_d() {
        for (moved, by) in moves_of_d() {
            let runs = walks_within(Some(3), across(false), (walk, kind), vec![moved]);
            for child in children {
                assert!(
                    runs.iter().all(|run| run.yields(child)),
                    "{} beside a {}: a walk left '{child}' out",
                    kind.name,
                    by.name
                );
            }
            if let Some(marked) = move_mark(by) {
                assert!(
                    runs.iter().any(|run| {
                        (children.iter()).any(|child| run.worded_while_walking(child, marked))
                    }),
                    "{} beside a {}: no mark landed while a walk was under way",
                    kind.name,
                    by.name
                );
            }
        }
    }
}

/// Beyond the contract, which runs a trusted move beside readers alone: it covers a walk's restart
/// that loses its place, which only a writer beside the move makes. A children walk of `d/` beside
/// a dirty move of `d/f`, the node it stands on, into `e`, and a detach of `d/g`, the node its
/// restart resumes after: a walk that restarted past `g` and found it gone reaches the end and
/// walks again from the head (`ChainWalk::restart_at_end`), each iterator yielding `d/x` twice, as
/// it reports it may, and each collection through one answering `d/g` before `d/x`, as it keeps a
/// repeated child at its last place. Within three preemptions a run, which reach that order. The
/// walk from the parent's record walks as the one from the directory.
#[test]
fn a_walk_whose_restart_lost_its_place_walks_again_from_the_head() {
    let [iterator, _, named_iterator, collection, named_collection] = walks_of_d();
    for (walk, kind) in [iterator, named_iterator, collection, named_collection] {
        let runs = walks_within(
            Some(3),
            tree(&["d/f", "d/g", "d/x", "e/"]),
            (walk, kind),
            vec![
                |tree| Box::pin(async move { dirty_moved(&tree, "d/f", "e").await }),
                |tree| {
                    Box::pin(
                        async move { assert!(ops::unserved_detach(&tree, tree.id("d/g")).await) },
                    )
                },
            ],
        );
        let twice = |run: &WalkRun| run.yielded.iter().filter(|path| *path == "d/x").count() == 2;
        if report_repeats(&runs) {
            assert!(
                runs.iter()
                    .any(|run| run.repeats == Repeats::May && twice(run)),
                "{}: no walk yielded 'd/x' twice",
                kind.name
            );
        } else {
            assert!(
                runs.iter().any(|run| run.yielded == ["d/g", "d/x"]),
                "{}: no walk met 'd/x' again after 'd/g'",
                kind.name
            );
        }
    }
}

/// Each children walk of `d/` beside a served rename of `d/f` to another spelling, which keeps its
/// name's hash and moves it to the chain's head: a walk past the head misses it, reporting it may
/// have repeated, which the rule excuses for a node a `Move` within the directory repositioned;
/// `d/g` in every run.
#[test]
fn a_walk_beside_a_reposition_may_pass_the_node_over() {
    for (walk, kind) in walks_of_d() {
        let runs = walks_beside(
            served(&["d/f", "d/g"]),
            (walk, kind),
            vec![|tree| {
                Box::pin(async move {
                    let (d, f) = (tree.id("d"), tree.id("d/f"));
                    assert!(ops::served_rename(&tree, f, d, "F").await);
                })
            }],
        );
        assert!(runs.iter().all(|run| run.yields("d/g")), "{}", kind.name);
        assert!(
            runs.iter().any(|run| {
                run.repeats.may_pass_over() && run.moved_while_walking("d/f") && !run.yields("d/f")
            }),
            "{}: no walk passed the repositioned 'd/f' over",
            kind.name
        );
    }
}

/// A children walk of `d/` beside a served rename of `d/g` onto `d/f`, replacing it (C7): `d/g`,
/// the chain's head before the move and after it, yielded in every run; the replaced `d/f` in some
/// runs and not in others, the move landing while some walks were under way.
#[test]
fn a_walk_beside_a_replacing_rename_yields_what_the_model_holds() {
    let walk: OpFn = |tree| {
        Box::pin(async move {
            let _ = ops::children(&tree, tree.id("d")).await;
        })
    };
    let runs = walks_beside(
        served(&["d/f", "d/g"]),
        (walk, &catalogue::CHILDREN),
        vec![|tree| {
            Box::pin(async move {
                let (d, f, g) = (tree.id("d"), tree.id("d/f"), tree.id("d/g"));
                assert!(ops::served_rename_replacing(&tree, g, (d, "f"), f).await);
            })
        }],
    );
    assert!(
        runs.iter().all(|run| run.yields("d/g")),
        "a walk left 'd/g' out"
    );
    let with = runs.iter().filter(|run| run.yields("d/f")).count();
    assert!(
        with > 0 && with < runs.len(),
        "'d/f' yielded in {with} of {} runs",
        runs.len()
    );
    assert!(
        runs.iter().any(|run| run.moved_while_walking("d/g")),
        "no move while a walk was under way"
    );
}

/// A children walk of `p/` beside a detach of `p` itself: the walk reads the detached directory's
/// chain, which still holds `d`, so it yields `p/d` in every run, some begun after the detach.
#[test]
fn a_walk_of_a_detached_directory_yields_its_chain() {
    let walk: OpFn = |tree| {
        Box::pin(async move {
            let _ = ops::children(&tree, tree.id("p")).await;
        })
    };
    let runs = walks_beside(
        tree(&["p/d/"]),
        (walk, &catalogue::CHILDREN),
        vec![|tree| {
            Box::pin(async move { assert!(ops::unserved_detach(&tree, tree.id("p")).await) })
        }],
    );
    assert!(
        runs.iter().all(|run| run.yields("p/d")),
        "a walk left 'p/d' out"
    );
    assert!(
        runs.iter()
            .any(|run| run.gone_at("p").is_some_and(|seq| seq < run.head)),
        "no walk began after the detach"
    );
}

/// A children walk of a directory whose chain crosses blocks: it yields each child once, its record
/// read from the block holding it.
#[test]
fn a_walk_across_blocks_yields_every_child() {
    let walk: OpFn = |tree| {
        Box::pin(async move {
            let _ = ops::children(&tree, tree.id("d")).await;
        })
    };
    let runs = walks_beside(across(false), (walk, &catalogue::CHILDREN), vec![]);
    assert!(
        runs.iter()
            .all(|run| run.yielded == ["d/y", "d/x", "d/g", "d/f"]),
        "a walk left a child out"
    );
}

/// A children walk of `s/t/` beside a C API delete of `s/t`, which marks its subtree `Discarded`
/// in place with no change to `t`'s count: `s/t/f` yielded in some runs and skipped in others, in
/// some marked after the walk's head read with the walk reporting no repeat, which the rule
/// excuses for a node a record marked `Discarded` in place.
#[test]
fn a_walk_beside_a_pending_discard_skips_what_it_marked() {
    let setup: Setup<Tree> = Box::new(|| {
        Box::pin(async {
            let mut tree = staged_directory()().await;
            let f = ops::staged_add(&tree, tree.id("s/t"), file("f"), "f")
                .await
                .expect("'s/t/f' adds");
            tree.nodes.push(("s/t/f".to_string(), f));
            tree
        })
    });
    let walk: OpFn = |tree| {
        Box::pin(async move {
            let _ = ops::children(&tree, tree.id("s/t")).await;
        })
    };
    let runs = walks_beside(
        setup,
        (walk, &catalogue::CHILDREN),
        vec![|tree| {
            Box::pin(async move {
                let place = (tree.id("s/t"), tree.id("s"), hash_string("t"));
                assert!(ops::pend(&tree, place, async {}).await);
            })
        }],
    );
    let with = runs.iter().filter(|run| run.yields("s/t/f")).count();
    assert!(
        with > 0 && with < runs.len(),
        "'s/t/f' yielded in {with} of {} runs",
        runs.len()
    );
    assert!(
        runs.iter().any(|run| {
            run.repeats == Repeats::No && !run.yields("s/t/f") && run.gone_while_walking("s/t/f")
        }),
        "no walk reporting no repeat skipped 's/t/f' marked after its head read"
    );
}

/// A tree whose directory `d/` holds `f` and `g` in the first block and `x` and `y` in a later one,
/// each prepended to its chain as it is made, so the chain runs `y`, `x`, `g`, `f`, crossing the
/// blocks at `g`: the one point where a listing's walk, which reads a block's run of its chain
/// under one read lock, lets others in. Served where `served`.
fn across(served: bool) -> Setup<Tree> {
    across_after(served, |_| Box::pin(async {}))
}

/// [`across`] after `then` changed it, unrecorded, as a setup does, before it is served.
fn across_after(served: bool, then: Then) -> Setup<Tree> {
    Box::new(move || {
        Box::pin(async move {
            let tree = Tree::spread(&["d/f", "d/g", "e/"], "fill", &["d/x", "d/y"]).await;
            then(&tree).await;
            if served {
                tree.state.mark_served();
            }
            tree
        })
    })
}

/// Each listing of `d/` by its walk of the chain, with its kind.
fn listings_of_d() -> [(OpFn, &'static Kind); 2] {
    [
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::listed_children(&tree, tree.id("d")).await;
                })
            },
            &catalogue::LISTED_CHILDREN,
        ),
        (
            |tree| {
                Box::pin(async move {
                    let _ = ops::listed_name_hashes(&tree, tree.id("d")).await;
                })
            },
            &catalogue::LISTED_NAME_HASHES,
        ),
    ]
}

/// Each listing of a `d/` whose chain crosses blocks beside a create in `d`, which prepends past
/// the walk once it began: the created node listed in some runs and not in others, in some
/// published after the walk's head read and not listed.
#[test]
fn a_listing_beside_a_create_lists_what_the_model_holds() {
    for (listing, kind) in listings_of_d() {
        let runs = walks_beside(
            across(false),
            (listing, kind),
            vec![|tree| {
                Box::pin(async move {
                    assert!(
                        ops::checked_create(&tree, tree.id("d"), "n")
                            .await
                            .is_some()
                    );
                })
            }],
        );
        let with = (runs.iter())
            .filter(|run| run.yielded.iter().any(|path| !path.starts_with("d/")))
            .count();
        assert!(
            with > 0 && with < runs.len(),
            "{}: the created node listed in {with} of {} runs",
            kind.name,
            runs.len()
        );
        assert!(
            runs.iter().any(WalkRun::published_unyielded),
            "{}: no create landed past a listing",
            kind.name
        );
    }
}

/// Each listing of a `d/` whose chain crosses blocks beside a detach of `d/g`, where it crosses:
/// listed in some runs and not in others, in some detached while the walk was between the blocks;
/// every other child listed in every run.
#[test]
fn a_listing_beside_a_detach_at_a_block_boundary_lists_what_the_model_holds() {
    for (listing, kind) in listings_of_d() {
        let runs = walks_beside(
            across(false),
            (listing, kind),
            vec![|tree| {
                Box::pin(async move { assert!(ops::unserved_detach(&tree, tree.id("d/g")).await) })
            }],
        );
        let with = runs.iter().filter(|run| run.yields("d/g")).count();
        assert!(
            with > 0 && with < runs.len(),
            "{}: 'd/g' listed in {with} of {} runs",
            kind.name,
            runs.len()
        );
        assert!(
            runs.iter().any(|run| run.gone_while_walking("d/g")),
            "{}: no detach while a walk was under way",
            kind.name
        );
        for other in ["d/f", "d/x", "d/y"] {
            assert!(
                runs.iter().all(|run| run.yields(other)),
                "{}: a listing left '{other}' out",
                kind.name
            );
        }
    }
}

/// Each listing of a `d/` whose chain crosses blocks beside a move of `d/g`, where it crosses, into
/// `e`, by each of the trusted moves: a walk meeting it under `e` starts again past the last child
/// it listed (`ChainWalk::step`), and lists every other child of `d` in every run.
#[test]
fn a_listing_beside_a_move_at_a_block_boundary_starts_again() {
    let moves: [(OpFn, &'static Kind); 3] = moves_into_e!("d/g");
    for (listing, kind) in listings_of_d() {
        for (moved, by) in moves {
            let runs = walks_beside(across(false), (listing, kind), vec![moved]);
            assert!(
                runs.iter().any(|run| run.restarts > 0
                    && run.moved_while_walking("d/g")
                    && !run.yields("d/g")),
                "{} beside a {}: no listing started again past 'd/g' as it moved",
                kind.name,
                by.name
            );
            for other in ["d/f", "d/x", "d/y"] {
                assert!(
                    runs.iter().all(|run| run.yields(other)),
                    "{} beside a {}: a listing left '{other}' out",
                    kind.name,
                    by.name
                );
            }
        }
    }
}

/// Each listing of a `d/` whose chain crosses blocks beside a dirty move renaming `d/g` within `d`,
/// where it crosses, in place, which keeps its position and changes its name's hash: `d/g` listed
/// in every run, the listing with hashes answering one name's or the other's, in some renamed
/// while the walk was between the blocks.
#[test]
fn a_listing_beside_a_rename_at_a_block_boundary_lists_the_node_by_either_name() {
    for (listing, kind) in listings_of_d() {
        let runs = walks_beside(
            across(false),
            (listing, kind),
            vec![|tree| Box::pin(async move { renamed_within(&tree, "d/g", "h").await })],
        );
        assert!(runs.iter().all(|run| run.yields("d/g")), "{}", kind.name);
        assert!(
            runs.iter().any(|run| run.moved_while_walking("d/g")),
            "{}: no rename while a walk was under way",
            kind.name
        );
    }
}

/// Each listing of a `d/` whose chain crosses blocks beside a stage's respelling of `d/g`, where it
/// crosses, to `G`, made outside a claim, which a reader beside it cannot tell: the node keeps its
/// place and its hash, so `d/g` is listed in every run, in some respelled while the walk was
/// between the blocks.
#[test]
fn a_listing_beside_a_respelling_at_a_block_boundary_lists_the_node_in_every_run() {
    for (listing, kind) in listings_of_d() {
        let runs = walks_beside(
            across(false),
            (listing, kind),
            vec![|tree| {
                Box::pin(async move { ops::stage_respell(&tree, tree.id("d/g"), "G").await })
            }],
        );
        assert!(runs.iter().all(|run| run.yields("d/g")), "{}", kind.name);
        assert!(
            runs.iter().any(|run| run.moved_while_walking("d/g")),
            "{}: no respelling while a walk was under way",
            kind.name
        );
    }
}

/// Each listing of a served `d/` whose chain crosses blocks beside a rename of `d/f`, the chain's
/// tail, to another spelling, which moves it to the chain's head: a walk past the head misses it,
/// which the rule excuses for a node a `Move` within the directory repositioned behind a walk that
/// may have repeated.
#[test]
fn a_listing_beside_a_reposition_may_pass_the_node_over() {
    for (listing, kind) in listings_of_d() {
        let runs = walks_beside(
            across(true),
            (listing, kind),
            vec![|tree| {
                Box::pin(async move {
                    let (f, d) = (tree.id("d/f"), tree.id("d"));
                    assert!(ops::served_rename(&tree, f, d, "F").await);
                })
            }],
        );
        assert!(
            runs.iter()
                .any(|run| run.moved_while_walking("d/f") && !run.yields("d/f")),
            "{}: no listing passed the repositioned 'd/f' over",
            kind.name
        );
    }
}

/// Beyond the contract, which runs a trusted move beside readers alone: it covers a listing's
/// restart that loses its place, which only a writer beside the move makes. Each listing of a `d/`
/// whose chain crosses blocks beside a dirty move of `d/g`, where it crosses, into `e` and a detach
/// of `d/x`, the child its restart resumes after: a walk that restarted past it and found it gone
/// reaches the end and walks again from the head (`ChainWalk::restart_at_end`), arming a restart
/// each time and meeting `d/y` twice, which the model holds the listing to listing once. Within
/// four preemptions a run, which land both changes at the crossing.
#[test]
fn a_listing_whose_restart_lost_its_place_lists_each_child_once() {
    for (listing, kind) in listings_of_d() {
        let runs = walks_within(
            Some(4),
            across(false),
            (listing, kind),
            vec![
                |tree| Box::pin(async move { dirty_moved(&tree, "d/g", "e").await }),
                |tree| {
                    Box::pin(
                        async move { assert!(ops::unserved_detach(&tree, tree.id("d/x")).await) },
                    )
                },
            ],
        );
        assert!(
            runs.iter().any(|run| run.restarts >= 2),
            "{}: no listing walked again from the head",
            kind.name
        );
    }
}

/// Each collection of `d/` leaving staged deletes out beside a delete of `d/f`, which stages it
/// with no change to `d`'s count: `d/f` collected in some runs and not in others, in some staged
/// after the walk's head read and passed over; `d/g` in every run.
#[test]
fn a_collection_beside_a_staged_delete_leaves_out_what_it_staged() {
    let [.., collection, named_collection] = walks_of_d();
    for (collect, kind) in [collection, named_collection] {
        let runs = walks_beside(
            tree(&["d/f", "d/g"]),
            (collect, kind),
            vec![|tree| {
                Box::pin(async move {
                    ops::staged_delete_level(&tree, tree.id("d/f"), tree.id("d")).await;
                })
            }],
        );
        filtered(&runs, ("d/f", false), Node::is_staged_delete);
    }
}

/// Each collection of `d/` leaving staged deletes out beside a restore of `d/f`, staged for
/// deletion: `d/f` collected in some runs and not in others, in some restored after the walk's
/// head read and collected; `d/g` in every run.
#[test]
fn a_collection_beside_a_restore_keeps_what_it_restored() {
    let [.., collection, named_collection] = walks_of_d();
    for (collect, kind) in [collection, named_collection] {
        let runs = walks_beside(
            changed(&["d/f", "d/g"], |t| {
                Box::pin(ops::staged_delete_level(t, t.id("d/f"), t.id("d")))
            }),
            (collect, kind),
            vec![|tree| {
                Box::pin(async move {
                    assert!(ops::restore(&tree, tree.id("d/f"), &Content::default()).await);
                })
            }],
        );
        filtered(&runs, ("d/f", true), |words| !words.is_staged_delete());
    }
}

/// A tree of `d/f`, `d/g` and `d/s`, `s` a symbolic link, as a written one is marked.
fn with_symlink() -> Setup<Tree> {
    changed(&["d/f", "d/g", "d/s"], |t| {
        Box::pin(ops::mark_dirty_as(t, t.id("d/s"), NodeExtFlags::Symlink))
    })
}

/// A collection of `d/` leaving symbolic links out beside a scan's mark of `d/s`, a symbolic link
/// the mark finds a file: `d/s` collected in some runs and not in others, in some marked after the
/// walk's head read and collected; `d/g` in every run.
#[test]
fn a_versioned_collection_beside_a_mark_keeps_what_stopped_being_a_link() {
    let collect: OpFn = |tree| {
        Box::pin(async move {
            let _ = ops::collected_versioned_children(&tree, tree.id("d")).await;
        })
    };
    let runs = walks_beside(
        with_symlink(),
        (collect, &catalogue::COLLECTED_VERSIONED_CHILDREN),
        vec![|tree| Box::pin(async move { ops::scan_mark_modified(&tree, tree.id("d/s")).await })],
    );
    filtered(&runs, ("d/s", true), Filter::VERSIONED.keep);
}

/// A collection of `d/` keeping staged deletes beside a delete of `d/f`: `d/f` collected in every
/// run, staged for deletion or not, and the symbolic link `d/s` in none.
#[test]
fn a_versioned_collection_beside_a_staged_delete_keeps_what_it_staged() {
    let collect: OpFn = |tree| {
        Box::pin(async move {
            let _ = ops::collected_versioned_children(&tree, tree.id("d")).await;
        })
    };
    let runs = walks_beside(
        with_symlink(),
        (collect, &catalogue::COLLECTED_VERSIONED_CHILDREN),
        vec![|tree| {
            Box::pin(async move {
                ops::staged_delete_level(&tree, tree.id("d/f"), tree.id("d")).await;
            })
        }],
    );
    assert!(
        runs.iter()
            .any(|run| run.worded_before_walking("d/f", Node::is_staged_delete)),
        "no delete before a walk's head read"
    );
    assert!(
        runs.iter()
            .any(|run| run.worded_while_walking("d/f", Node::is_staged_delete)),
        "no delete while a walk was under way"
    );
    assert!(
        runs.iter()
            .all(|run| run.yields("d/f") && !run.yields("d/s")),
        "a walk left 'd/f' out or collected 'd/s'"
    );
}

/// Asserts of a collection's `runs` beside a change of the child at `path` that flips whether its
/// filter keeps it, to kept where `kept`: the child collected in some runs and not in others, in
/// some given words `flipped` holds of after the walk's head read and collected as those words
/// say, and `d/g` collected in every run.
fn filtered(runs: &[WalkRun], (path, kept): (&str, bool), flipped: fn(&Node) -> bool) {
    let with = runs.iter().filter(|run| run.yields(path)).count();
    assert!(
        with > 0 && with < runs.len(),
        "'{path}' collected in {with} of {} runs",
        runs.len()
    );
    assert!(
        runs.iter()
            .any(|run| run.worded_while_walking(path, flipped) && run.yields(path) == kept),
        "no walk met '{path}' as its words flipped after its head read"
    );
    assert!(
        runs.iter().all(|run| run.yields("d/g")),
        "a walk left 'd/g' out"
    );
}

/// What a listing reader's change made in one run of an exploration: its outcome; the paths of the
/// children its build listed; whether its attach or move went through its absence, by the check
/// its record names, and whether one was refused through it, the log since the lookup holding the
/// name; the arms its call holds; and the outcomes of the other tasks' calls.
struct ListingRun {
    outcome: Outcome,
    listed: Vec<String>,
    through_absence: bool,
    refused_through_absence: bool,
    arms: Vec<Arm>,
    others: Vec<Outcome>,
}

impl ListingRun {
    /// How many of the run's calls did what they were called for.
    fn done_calls(&self) -> usize {
        (std::iter::once(&self.outcome).chain(&self.others))
            .filter(|outcome| matches!(outcome, Outcome::Done(_)))
            .count()
    }

    /// Whether the listing reader's change was done.
    fn made(&self) -> bool {
        matches!(self.outcome, Outcome::Done(_))
    }

    /// Whether its build listed the node at `path`.
    fn listed(&self, path: &str) -> bool {
        self.listed.iter().any(|listed| listed == path)
    }
}

/// Whether `effect` is a publish or a move checked against an absence.
pub(crate) fn through_absence(effect: &Effect) -> bool {
    matches!(
        effect,
        Effect::Publish {
            check: Check::Absent,
            ..
        } | Effect::Move {
            check: Check::Absent,
            ..
        }
    )
}

/// Whether another task's record of `run` came within `call`, between its first head read of its
/// target and its end: a change landing after a listing began.
fn met_after_head(run: &Run<'_>, call: &Call) -> bool {
    let dir = call.target.expect("the call names its directory");
    let listed = (call.first_head(dir, &run.records, &run.spawns))
        .expect("the call read its directory's head");
    (run.records.iter()).any(|record| {
        record.state == call.state
            && record.task != call.task
            && listed < record.seq
            && record.seq < call.end
    })
}

/// Whether `effect` counts on directory `dir`'s chain: a child published, unlinked or renamed
/// there, or moved in or out.
pub(crate) fn changes_chain_of(effect: &Effect, dir: NodeID) -> bool {
    match *effect {
        Effect::Publish { parent, .. }
        | Effect::Detach { parent, .. }
        | Effect::Rename { parent, .. } => parent == dir,
        Effect::Move { from, to, .. } => from == dir || to == dir,
        _ => false,
    }
}

/// Whether `effect` is a refusal on a holder the log since an absence named.
pub(crate) fn refused_through_absence(effect: &Effect) -> bool {
    matches!(
        effect,
        Effect::Refused {
            reason: Reason::Exists,
            expected: Expected::Log { .. },
            ..
        }
    )
}

/// The path the tree built `node` at, or the node's id where it built none.
fn path_of(tree: &Tree, node: NodeID) -> String {
    (tree.nodes.iter())
        .find(|&&(_, built)| built == node)
        .map_or_else(|| node.to_string(), |(path, _)| path.clone())
}

/// Explores `change`, a listing reader's change of `kind`'s, beside `changes` on the tree `setup`
/// builds, within `preemptions` per run where given ([`explored_within`]). What the change made in
/// each run.
fn listing_changes_within(
    preemptions: Option<usize>,
    setup: Setup<Tree>,
    (change, kind): (OpFn, &'static Kind),
    changes: Vec<OpFn>,
) -> Vec<ListingRun> {
    explored_within(preemptions, setup, (change, kind), changes, |run, call| {
        let own: Vec<&Effect> = (run.records.iter())
            .filter(|record| call.holds(record, &run.spawns))
            .map(|record| &record.effect)
            .collect();
        let listed = (call.walks.iter())
            .flat_map(|walk| walk.yielded.iter())
            .map(|yielded| path_of(run.tree, yielded.node))
            .collect();
        let arms = (own.iter())
            .filter_map(|effect| match effect {
                Effect::Arm(arm) => Some(*arm),
                _ => None,
            })
            .collect();
        let others = (run.calls.iter())
            .filter(|other| other.task != call.task)
            .map(|other| other.outcome.clone())
            .collect();
        ListingRun {
            outcome: call.outcome.clone(),
            listed,
            through_absence: own.iter().any(|effect| through_absence(effect)),
            refused_through_absence: own.iter().any(|effect| refused_through_absence(effect)),
            arms,
            others,
        }
    })
}

/// A listing reader's create of `d/n`.
fn listing_create_n() -> (OpFn, &'static Kind) {
    (
        |tree| {
            Box::pin(async move {
                let _ = ops::listing_create(&tree, tree.id("d"), "n").await;
            })
        },
        &catalogue::LISTING_CREATE,
    )
}

/// A listing reader's rename of `d/f` to `n` in `d`.
fn listing_rename_f_to_n() -> (OpFn, &'static Kind) {
    (
        |tree| {
            Box::pin(async move {
                let (f, d) = (tree.id("d/f"), tree.id("d"));
                let _ = ops::listing_rename(&tree, f, d, "n").await;
            })
        },
        &catalogue::LISTING_RENAME,
    )
}

/// Another task's served create of `d/n`.
fn served_create_n() -> OpFn {
    |tree| {
        Box::pin(async move {
            let _ = ops::served_create(&tree, tree.id("d"), "n").await;
        })
    }
}

/// Each listing reader's change taking the name `n` in `d` beside another task's served create of
/// `d/n`: one of them takes the name in every run, the model holding it unique (C8); in some runs
/// the listing reader's change goes through its absence, and in some the log since its lookup holds
/// the create's attach, which its check finds. Within five preemptions a run, which reach the
/// create between the lookup and the change.
#[test]
fn a_listing_change_beside_a_create_of_its_name_takes_it_or_finds_it_held() {
    for change in [listing_create_n(), listing_rename_f_to_n()] {
        let name = change.1.name;
        let runs =
            listing_changes_within(Some(5), served(&["d/f"]), change, vec![served_create_n()]);
        assert!(
            runs.iter().all(|run| run.done_calls() == 1),
            "{name}: a run took the name twice or not at all"
        );
        assert!(
            runs.iter().any(|run| run.through_absence && run.made()),
            "{name}: no change went through its absence"
        );
        assert!(
            runs.iter().any(|run| run.refused_through_absence),
            "{name}: no change found the name its absence missed"
        );
    }
}

/// A listing reader's create of `d/n` beside another task's served detach of `d/n`: refused where
/// its attach found `n` still held, attached once it was gone, by walking the chain where its
/// lookup found `n` still held, and through the absence the log since the build shows in some runs
/// whose build listed `n` before the detach. Every schedule: eight preemptions a run reach them
/// all.
#[test]
fn a_listing_create_beside_a_detach_of_its_name_attaches_once_it_is_gone() {
    let runs = listing_changes_within(
        Some(8),
        served(&["d/f", "d/n"]),
        listing_create_n(),
        vec![|tree| {
            Box::pin(async move {
                assert!(ops::served_detach(&tree, tree.id("d/n")).await);
            })
        }],
    );
    assert!(
        runs.iter().all(|run| matches!(
            run.outcome,
            Outcome::Done(_) | Outcome::Refused(Refusal::Exists)
        )),
        "a listing create answered otherwise"
    );
    assert!(
        runs.iter()
            .any(|run| matches!(run.outcome, Outcome::Refused(Refusal::Exists))),
        "no listing create found 'n' held"
    );
    assert!(
        runs.iter().any(|run| run.made() && !run.through_absence),
        "no listing create attached by walking the chain"
    );
    assert!(
        runs.iter()
            .any(|run| run.through_absence && run.made() && run.listed("d/n")),
        "no listing create that listed 'n' attached through its absence"
    );
}

/// Each listing reader's change taking the name `n` in `d` where another task creates `d/n` and
/// detaches it between the change's lookup and its attach or move: the log's newest patch of the
/// name since the lookup detaches it, so the change goes through its absence in every run.
#[test]
fn a_listing_change_after_its_name_came_and_went_goes_through_its_absence() {
    let changes: [(OpFn, &'static Kind); 2] = [
        (
            |tree| {
                Box::pin(async move {
                    let d = tree.id("d");
                    let windows = (async {}, came_and_went(&tree));
                    let created = ops::listing_create_waiting(&tree, d, "n", windows);
                    assert!(created.await.is_some(), "'d/n' attaches");
                })
            },
            &catalogue::LISTING_CREATE,
        ),
        (
            |tree| {
                Box::pin(async move {
                    let (f, d) = (tree.id("d/f"), tree.id("d"));
                    let windows = (async {}, came_and_went(&tree));
                    let renamed = ops::listing_rename_waiting(&tree, f, d, "n", windows);
                    assert!(renamed.await, "'d/f' moves to 'd/n'");
                })
            },
            &catalogue::LISTING_RENAME,
        ),
    ];
    for change in changes {
        let name = change.1.name;
        let runs = listing_changes_within(None, served(&["d/f"]), change, vec![]);
        assert!(
            runs.iter().all(|run| run.through_absence),
            "{name}: a change walked the chain"
        );
    }
}

/// A served create of `d/n`, then its served detach.
async fn came_and_went(tree: &Tree) {
    let n = ops::served_create(tree, tree.id("d"), "n").await;
    assert!(ops::served_detach(tree, n.expect("'d/n' attaches")).await);
}

/// Each listing reader's change taking the name `n` in `d` after more changes of `d` than its log
/// holds, made between its lookup and its attach or move: the log no longer covers its absence, so
/// the change takes the name by walking the chain, the attach arming its finding as
/// `AttachUncovered`, the move as `ReattachChanged`.
#[test]
fn a_listing_change_after_more_changes_than_its_log_holds_walks_the_chain() {
    let changes: [((OpFn, &'static Kind), Arm); 2] = [
        (
            (
                |tree| {
                    Box::pin(async move {
                        let d = tree.id("d");
                        let windows = (async {}, overflowing(&tree));
                        let created = ops::listing_create_waiting(&tree, d, "n", windows);
                        assert!(created.await.is_some(), "'d/n' attaches");
                    })
                },
                &catalogue::LISTING_CREATE,
            ),
            Arm::AttachUncovered,
        ),
        (
            (
                |tree| {
                    Box::pin(async move {
                        let (f, d) = (tree.id("d/f"), tree.id("d"));
                        let windows = (async {}, overflowing(&tree));
                        let renamed = ops::listing_rename_waiting(&tree, f, d, "n", windows);
                        assert!(renamed.await, "'d/f' moves to 'd/n'");
                    })
                },
                &catalogue::LISTING_RENAME,
            ),
            Arm::ReattachChanged,
        ),
    ];
    for (change, arm) in changes {
        let name = change.1.name;
        let runs = listing_changes_within(None, served(&["d/f"]), change, vec![]);
        assert!(
            runs.iter().all(|run| run.made() && !run.through_absence),
            "{name}: a change checked an absence its log no longer covered"
        );
        assert!(
            runs.iter().all(|run| run.arms.contains(&arm)),
            "{name}: a change took the name with no {arm:?}"
        );
    }
}

/// A listing reader's create of `d/n` whose build met more changes of `d` than its log holds
/// between its mark and its walk, beside another task's served create of `d/n`: the log no longer
/// covers the build, so its lookup issues no absence, and a create of `n` past the walk is found by
/// walking the chain, refused. Within two preemptions a run.
#[test]
fn a_listing_create_whose_log_overflowed_in_its_build_walks_the_chain() {
    let runs = listing_changes_within(
        Some(2),
        served(&["d/f"]),
        (
            |tree| {
                Box::pin(async move {
                    let d = tree.id("d");
                    let windows = (overflowing(&tree), async {});
                    let _ = ops::listing_create_waiting(&tree, d, "n", windows).await;
                })
            },
            &catalogue::LISTING_CREATE,
        ),
        vec![served_create_n()],
    );
    assert!(
        runs.iter()
            .all(|run| run.done_calls() == 1 && !run.through_absence),
        "a run took the name twice or not at all, or through an absence"
    );
    assert!(
        runs.iter()
            .any(|run| { run.outcome == Outcome::Refused(Refusal::Exists) && !run.listed("d/n") }),
        "no listing create found by walking a name its build did not list"
    );
}

/// Served creates in `d` of one more name than a directory's log first holds.
async fn overflowing(tree: &Tree) {
    let d = tree.id("d");
    for index in 0..=CHAIN_LOG_INITIAL {
        let name = format!("m{index}");
        assert!(ops::served_create(tree, d, &name).await.is_some());
    }
}

/// Each listing reader's change taking the name `n` in `e`, an empty directory, beside a served
/// RMDIR of `e`: in some runs the change takes the name and the RMDIR finds `e` not empty; in
/// others the change finds `e` gone, before its lookup or under its locks, where its attach arms
/// `AttachChanged` and its move `ReattachChanged`. Every schedule: five preemptions a run reach
/// them all.
#[test]
fn a_listing_change_beside_an_rmdir_of_its_directory_takes_the_name_or_finds_it_gone() {
    let changes: [((OpFn, &'static Kind), Arm); 2] = [
        (
            (
                |tree| {
                    Box::pin(async move {
                        let _ = ops::listing_create(&tree, tree.id("e"), "n").await;
                    })
                },
                &catalogue::LISTING_CREATE,
            ),
            Arm::AttachChanged,
        ),
        (
            (
                |tree| {
                    Box::pin(async move {
                        let (f, e) = (tree.id("d/f"), tree.id("e"));
                        let _ = ops::listing_rename(&tree, f, e, "n").await;
                    })
                },
                &catalogue::LISTING_RENAME,
            ),
            Arm::ReattachChanged,
        ),
    ];
    for (change, arm) in changes {
        let name = change.1.name;
        let runs = listing_changes_within(
            Some(5),
            served(&["d/f", "e/"]),
            change,
            vec![|tree| {
                Box::pin(async move {
                    let _ = ops::served_rmdir(&tree, tree.id("e")).await;
                })
            }],
        );
        let gone = |run: &ListingRun| run.outcome == Outcome::Refused(Refusal::NotFound);
        assert!(
            runs.iter().all(|run| run.done_calls() == 1),
            "{name}: both the change and the RMDIR done, or neither"
        );
        assert!(
            runs.iter()
                .any(|run| run.made() && run.others == [Outcome::Refused(Refusal::NotEmpty)]),
            "{name}: no change took the name before the RMDIR"
        );
        assert!(runs.iter().any(gone), "{name}: no change found 'e' gone");
        assert!(
            runs.iter().any(|run| gone(run) && run.arms.contains(&arm)),
            "{name}: no change found 'e' gone under its locks"
        );
    }
}

/// A listing reader's create of `d/n` in a directory holding a child staged for deletion, which
/// the build leaves out: no absence, so the attach walks the chain.
#[test]
fn a_listing_create_beside_a_staged_child_walks_the_chain() {
    let runs = listing_changes_within(
        None,
        changed(&["d/f", "d/g"], |t| {
            Box::pin(async move {
                ops::staged_delete_level(t, t.id("d/f"), t.id("d")).await;
                t.state.mark_served();
            })
        }),
        listing_create_n(),
        vec![],
    );
    assert!(
        (runs.iter()).all(|run| run.made() && !run.through_absence),
        "a listing create attached through an absence its build could not show"
    );
}
