// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The scheduler's own exploration: steps writing one block are tried in every order, and steps
//! writing blocks apart are pruned as reorderings of one another. With it, the checks only a
//! `state-hooks` build carries: the lock order and the pending clear's write guard.

use std::sync::Arc;

use lore_revision::node::NodeBlock;
use lore_revision::state::node::hooks;
use lore_revision::state::node::hooks::Point;

use crate::harness::fixtures::Tree;
use crate::harness::invariants;
use crate::harness::scheduler::Covered;
use crate::harness::scheduler::Exploration;
use crate::harness::scheduler::Op;
use crate::harness::scheduler::Scenario;

/// Write-locks the block of `path`, parks, and write-locks it again.
fn writes(path: &'static str) -> Op<Tree> {
    Box::new(move |tree: Arc<Tree>| {
        Box::pin(async move {
            let block = invariants::block_of(&tree.state, &tree.repository, tree.id(path)).await;
            drop(block.write_for_tests());
            hooks::reached(Point::ClearPhase).await;
            drop(block.write_for_tests());
        })
    })
}

/// The runs exploring two tasks that write the block of `a` and of `b`, in `tree`, where those
/// blocks are one block or not as `shared` says.
fn runs(tree: fn() -> std::pin::Pin<Box<dyn Future<Output = Tree>>>, shared: bool) -> usize {
    let scenario = Scenario::<Tree> {
        name: "scheduler: two tasks writing blocks",
        setup: Box::new(tree),
        ops: vec![writes("a"), writes("b")],
        cancellable: vec![],
        check: Box::new(move |tree: Arc<Tree>| {
            Box::pin(async move {
                let (a, b) = (
                    NodeBlock::index(tree.id("a")),
                    NodeBlock::index(tree.id("b")),
                );
                assert_eq!(a == b, shared, "blocks {a} and {b}");
            })
        }),
    };
    let covered = scenario.explore(Exploration::default());
    assert!(
        covered.complete,
        "explored {} runs without finishing",
        covered.runs
    );
    covered.runs
}

/// A tree and the order its two tasks' steps were taken in.
struct Ordered {
    tree: Tree,
    order: std::sync::Mutex<Vec<usize>>,
}

/// Task `task`'s two steps, each writing the block of `a` and noting the task.
fn ordered_steps(task: usize) -> Op<Ordered> {
    Box::new(move |ordered: Arc<Ordered>| {
        Box::pin(async move {
            let tree = &ordered.tree;
            let block = invariants::block_of(&tree.state, &tree.repository, tree.id("a")).await;
            for _ in 0..2 {
                drop(block.write_for_tests());
                ordered.order.lock().expect("the order").push(task);
                hooks::reached(Point::ClearPhase).await;
            }
        })
    })
}

/// The orders two tasks of [`ordered_steps`] ran in, explored as `exploration` says, and what the
/// exploration covered.
fn orders(exploration: Exploration) -> (std::collections::BTreeSet<Vec<usize>>, Covered) {
    let seen = Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::new()));
    let into = seen.clone();
    let scenario = Scenario::<Ordered> {
        name: "scheduler: two tasks' steps in order",
        setup: Box::new(|| {
            Box::pin(async {
                Ordered {
                    tree: Tree::new(&["a/"]).await,
                    order: std::sync::Mutex::default(),
                }
            })
        }),
        ops: vec![ordered_steps(0), ordered_steps(1)],
        cancellable: vec![],
        check: Box::new(move |ordered: Arc<Ordered>| {
            let into = into.clone();
            Box::pin(async move {
                let order = ordered.order.lock().expect("the order").clone();
                into.lock().expect("the orders").insert(order);
            })
        }),
    };
    let covered = scenario.explore(exploration);
    let orders = seen.lock().expect("the orders").clone();
    (orders, covered)
}

/// An exploration at its preemption bound alone reaches every order deepening it reaches, in
/// fewer runs: it runs no schedule of a lower bound again.
#[test]
fn a_single_pass_at_the_bound_reaches_every_order_in_fewer_runs() {
    let explored = |deepening| {
        orders(Exploration {
            deepening,
            ..Exploration::default()
        })
    };
    let (deepened, deepening) = explored(true);
    let (single, single_pass) = explored(false);
    assert!(
        deepening.complete && single_pass.complete,
        "{deepening:?}, {single_pass:?}"
    );
    assert_eq!(
        single.len(),
        6,
        "every interleaving of two two-step tasks: {single:?}"
    );
    assert_eq!(single, deepened, "the orders each reached");
    assert!(
        single_pass.runs < deepening.runs,
        "{} runs at the bound alone, {} deepening",
        single_pass.runs,
        deepening.runs
    );
}

/// An exploration within no preemption runs each task to its end before the other, and covers
/// that bound, not every interleaving.
#[test]
fn an_exploration_without_preemptions_runs_each_task_to_its_end_first() {
    let (reached, covered) = orders(Exploration {
        preemptions: 0,
        deepening: false,
        sleep_sets: false,
        ..Exploration::default()
    });
    assert!(
        !covered.complete && covered.bound == Some(0) && !covered.timed_out,
        "{covered:?}"
    );
    assert_eq!(
        reached,
        std::collections::BTreeSet::from([vec![0, 0, 1, 1], vec![1, 1, 0, 0]])
    );
}

#[test]
fn steps_writing_one_block_are_tried_in_more_orders_than_steps_writing_two() {
    let shared = runs(|| Box::pin(Tree::new(&["a/", "b/"])), true);
    let apart = runs(|| Box::pin(Tree::spread(&["a/"], "fill", &["b/"])), false);
    assert!(
        apart < shared,
        "blocks apart took {apart} runs, one block {shared}"
    );
}

/// A thread taking a block's guard while it holds one of a later block panics, before the lock
/// could deadlock against a change taking both in order (C13).
#[test]
#[should_panic(expected = "blocks lock in ascending index")]
fn a_block_locked_out_of_order_panics() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("the runtime starts");
    runtime.block_on(lore_base::runtime::LORE_CONTEXT.scope(
        crate::harness::fixtures::execution(),
        async {
            let tree = Tree::spread(&["a/"], "fill", &["b/"]).await;
            let (a, b) = (
                invariants::block_of(&tree.state, &tree.repository, tree.id("a")).await,
                invariants::block_of(&tree.state, &tree.repository, tree.id("b")).await,
            );
            let _later = b.write_for_tests();
            let _earlier = a.read();
        },
    ));
}

/// Reads the size of `a`, parks, and writes it back one larger: a lost update where two run
/// interleaved.
fn increments() -> Op<Tree> {
    Box::new(move |tree: Arc<Tree>| {
        Box::pin(async move {
            let a = tree.id("a");
            let held = tree
                .state
                .node(tree.repository.clone(), a)
                .await
                .expect("a reads");
            hooks::reached(Point::ClearPhase).await;
            let block = invariants::block_of(&tree.state, &tree.repository, a).await;
            assert!(tree.state.node_change_in_place(
                &block,
                a,
                (held.parent, held.name_hash),
                |node| {
                    node.set_size(held.size + 1);
                    true
                }
            ));
        })
    })
}

/// The scheduler finds a bug only some interleaving shows: two read-modify-writes across a
/// scheduling point, which lose an update where both read before either writes.
#[test]
#[should_panic(expected = "an increment was lost")]
fn exploration_finds_a_lost_update() {
    let scenario = Scenario::<Tree> {
        name: "scheduler: two increments across a scheduling point",
        setup: Box::new(|| Box::pin(Tree::new(&["a/"]))),
        ops: vec![increments(), increments()],
        cancellable: vec![],
        check: Box::new(|tree: Arc<Tree>| {
            Box::pin(async move {
                let a = tree
                    .state
                    .node(tree.repository.clone(), tree.id("a"))
                    .await
                    .expect("a reads");
                assert_eq!(a.size, 2, "an increment was lost");
            })
        }),
    };
    scenario.explore(Exploration::default());
}

/// What the harness's own scenarios record: the steps their tasks took, in order.
#[derive(Default)]
struct Log(std::sync::Mutex<Vec<&'static str>>);

impl Log {
    fn push(&self, step: &'static str) {
        self.0.lock().expect("the log").push(step);
    }

    fn steps(&self) -> Vec<&'static str> {
        self.0.lock().expect("the log").clone()
    }
}

fn log_setup() -> crate::harness::scheduler::Setup<Log> {
    Box::new(|| Box::pin(async { Log::default() }))
}

/// Every interleaving, none pruned: the harness scenarios' steps touch nothing the hooks record.
fn unpruned() -> Exploration {
    Exploration {
        sleep_sets: false,
        ..Exploration::default()
    }
}

/// A future an operation spawns through `hooks::inherit` is a task of its own, parking at its
/// own points: another operation's step runs between two of its steps, though the operation
/// that spawned it parks nowhere while it awaits it.
#[test]
fn a_spawned_future_is_scheduled_as_a_task_of_its_own() {
    let orders = Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::new()));
    let seen = orders.clone();
    let spawns: Op<Log> = Box::new(|log: Arc<Log>| {
        Box::pin(async move {
            let child = lore_base::lore_spawn!(hooks::inherit(async move {
                log.push("child 1");
                hooks::reached(Point::Named("child")).await;
                log.push("child 2");
            }));
            child.await.expect("the child finishes");
        })
    });
    let other: Op<Log> = Box::new(|log: Arc<Log>| {
        Box::pin(async move {
            hooks::reached(Point::Named("other")).await;
            log.push("other");
        })
    });
    let scenario = Scenario::<Log> {
        name: "scheduler: a spawned future",
        setup: log_setup(),
        ops: vec![spawns, other],
        cancellable: vec![],
        check: Box::new(move |log: Arc<Log>| {
            let seen = seen.clone();
            Box::pin(async move {
                seen.lock().expect("the orders").insert(log.steps());
            })
        }),
    };
    let covered = scenario.explore(unpruned());
    assert!(covered.complete, "explored {} runs", covered.runs);
    let orders = orders.lock().expect("the orders");
    assert!(
        orders.contains(&vec!["child 1", "other", "child 2"]),
        "the other operation runs between the child's steps: {orders:?}"
    );
}

/// An operation dropped while the future it spawned into a set is parked drops that future
/// with it, where it parked: the run goes on with no task left waiting, and the future never
/// takes its next step. The run's check is told it dropped one.
#[test]
fn a_cancelled_operation_drops_the_future_it_awaits() {
    let outcomes = Arc::new([
        std::sync::atomic::AtomicUsize::new(0),
        std::sync::atomic::AtomicUsize::new(0),
    ]);
    let seen = outcomes.clone();
    let spawns: Op<Log> = Box::new(|log: Arc<Log>| {
        Box::pin(async move {
            let mut set = tokio::task::JoinSet::new();
            lore_base::lore_spawn!(
                set,
                hooks::inherit(async move {
                    hooks::reached(Point::Named("child")).await;
                    log.push("child done");
                })
            );
            set.join_next().await;
        })
    });
    let scenario = Scenario::<Log> {
        name: "scheduler: a cancelled operation and its child",
        setup: log_setup(),
        ops: vec![spawns],
        cancellable: vec![0],
        check: Box::new(move |log: Arc<Log>| {
            let seen = seen.clone();
            Box::pin(async move {
                let done = log.steps().contains(&"child done");
                assert_eq!(
                    crate::harness::scheduler::run_cancelled(),
                    !done,
                    "the check is told whether its run dropped the operation"
                );
                seen[usize::from(done)].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            })
        }),
    };
    let covered = scenario.explore(Exploration {
        cancellation: true,
        ..unpruned()
    });
    assert!(covered.complete, "explored {} runs", covered.runs);
    for (done, what) in [(false, "dropped"), (true, "finished")] {
        assert!(
            outcomes[usize::from(done)].load(std::sync::atomic::Ordering::Relaxed) > 0,
            "some run has the child {what}"
        );
    }
}

/// An operation parked at a point no await stands at outside the scheduler is never dropped
/// there, as no future can be dropped between two steps nothing separates; it is dropped at a
/// point an await stands at.
#[test]
fn a_cancel_drops_an_operation_only_where_an_await_stands() {
    let outcomes = Arc::new([
        std::sync::atomic::AtomicUsize::new(0),
        std::sync::atomic::AtomicUsize::new(0),
        std::sync::atomic::AtomicUsize::new(0),
    ]);
    let seen = outcomes.clone();
    let parks: Op<Log> = Box::new(|log: Arc<Log>| {
        Box::pin(async move {
            hooks::reached(Point::Named("awaits")).await;
            log.push("past the await");
            hooks::reached(Point::BeforeRegistration).await;
            log.push("registered");
        })
    });
    let scenario = Scenario::<Log> {
        name: "scheduler: a cancel where no await stands",
        setup: log_setup(),
        ops: vec![parks],
        cancellable: vec![0],
        check: Box::new(move |log: Arc<Log>| {
            let seen = seen.clone();
            Box::pin(async move {
                let steps = log.steps();
                let reached = usize::from(steps.contains(&"past the await"))
                    + usize::from(steps.contains(&"registered"));
                seen[reached].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            })
        }),
    };
    let covered = scenario.explore(Exploration {
        cancellation: true,
        ..unpruned()
    });
    assert!(covered.complete, "explored {} runs", covered.runs);
    let count = |reached: usize| outcomes[reached].load(std::sync::atomic::Ordering::Relaxed);
    assert!(count(0) > 0, "some run drops the operation at its await");
    assert_eq!(
        count(1),
        0,
        "no run drops it between its dirty mark and its registration"
    );
    assert!(count(2) > 0, "some run finishes it");
}

/// An operation is not dropped while any of its tasks is parked where no await stands, though
/// another of them is parked where one does: the cancel drops every task of the operation where
/// it is parked.
#[test]
fn a_cancel_waits_for_every_task_of_the_operation_to_stand_at_an_await() {
    let outcomes = Arc::new([
        std::sync::atomic::AtomicUsize::new(0),
        std::sync::atomic::AtomicUsize::new(0),
    ]);
    let seen = outcomes.clone();
    let spawns: Op<Log> = Box::new(|log: Arc<Log>| {
        Box::pin(async move {
            let mut set = tokio::task::JoinSet::new();
            let other = log.clone();
            lore_base::lore_spawn!(
                set,
                hooks::inherit(async move {
                    hooks::reached(Point::Named("awaits")).await;
                    other.push("awaited");
                })
            );
            lore_base::lore_spawn!(
                set,
                hooks::inherit(async move {
                    log.push("marked");
                    hooks::reached(Point::BeforeRegistration).await;
                    log.push("registered");
                })
            );
            while set.join_next().await.is_some() {}
        })
    });
    let scenario = Scenario::<Log> {
        name: "scheduler: a cancel with a task where no await stands",
        setup: log_setup(),
        ops: vec![spawns],
        cancellable: vec![0],
        check: Box::new(move |log: Arc<Log>| {
            let seen = seen.clone();
            Box::pin(async move {
                let steps = log.steps();
                if !steps.contains(&"awaited") {
                    seen[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                if steps.contains(&"marked") && !steps.contains(&"registered") {
                    seen[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            })
        }),
    };
    let covered = scenario.explore(Exploration {
        cancellation: true,
        ..unpruned()
    });
    assert!(covered.complete, "explored {} runs", covered.runs);
    let count = |outcome: usize| outcomes[outcome].load(std::sync::atomic::Ordering::Relaxed);
    assert!(count(0) > 0, "some run drops the operation");
    assert_eq!(
        count(1),
        0,
        "no run drops it between a task's dirty mark and its registration"
    );
}

/// A panic in a future an operation spawned fails the run, though the operation took it for an
/// error and went on, as the C API's verbs take a task that died for a failed entry.
#[test]
#[should_panic(expected = "spawned tasks [1] panicked")]
fn a_panic_in_a_spawned_future_fails_the_run() {
    let spawns: Op<Log> = Box::new(|_: Arc<Log>| {
        Box::pin(async move {
            let child = lore_base::lore_spawn!(hooks::inherit(async move {
                hooks::reached(Point::Named("child")).await;
                panic!("the child fails");
            }));
            let _ = child.await;
        })
    });
    let scenario = Scenario::<Log> {
        name: "scheduler: a panicking child",
        setup: log_setup(),
        ops: vec![spawns],
        cancellable: vec![],
        check: Box::new(|_: Arc<Log>| Box::pin(async {})),
    };
    scenario.explore(unpruned());
}

/// A task waiting on something outside the run's thread, with none parked, is waited for in
/// real time rather than called blocked, as a C API call waits for its event forwarder.
#[test]
fn a_task_waiting_outside_the_runs_thread_is_waited_for() {
    let waits: Op<Log> = Box::new(|log: Arc<Log>| {
        Box::pin(async move {
            hooks::reached(Point::Named("before")).await;
            let (sent, received) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(50));
                let _ = sent.send(());
            });
            received.await.expect("the thread sends");
            log.push("woken");
        })
    });
    let scenario = Scenario::<Log> {
        name: "scheduler: a wait outside the run's thread",
        setup: log_setup(),
        ops: vec![waits],
        cancellable: vec![],
        check: Box::new(|log: Arc<Log>| {
            Box::pin(async move {
                assert_eq!(log.steps(), vec!["woken"], "the task went on");
            })
        }),
    };
    let covered = scenario.explore(unpruned());
    assert!(covered.complete, "explored {} runs", covered.runs);
}

/// A task whose two futures reach scheduling points at once, which no node operation does, fails
/// the run rather than losing one of the two parks.
#[test]
#[should_panic(expected = "parked at once")]
fn two_futures_of_one_task_parked_at_once_fail_the_run() {
    let both: Op<Log> = Box::new(|_: Arc<Log>| {
        Box::pin(async move {
            tokio::join!(
                hooks::reached(Point::Named("one")),
                hooks::reached(Point::Named("two")),
            );
        })
    });
    let scenario = Scenario::<Log> {
        name: "scheduler: two parks of one task",
        setup: log_setup(),
        ops: vec![both],
        cancellable: vec![],
        check: Box::new(|_: Arc<Log>| Box::pin(async {})),
    };
    scenario.explore(unpruned());
}
