// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The deterministic scheduler: runs a scenario's operations as tasks on a `current_thread`
//! runtime, one step at a time, a step being what a task does between two scheduling points
//! (`lore_revision::state::node::hooks`), and explores the interleavings of those steps.
//!
//! Exploration is a depth-first search over the choice of which parked task runs next, run
//! again from a fresh setup for every interleaving. Sleep sets prune interleavings that only
//! reorder independent steps: two steps are dependent where both touch one resource and at
//! least one writes it. Scenarios whose steps touch state the hooks do not record, such as the
//! VFS provider's host files, turn the pruning off.

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;

use lore_revision::state::node::hooks;
use lore_revision::state::node::hooks::Point;
use lore_revision::state::node::hooks::Resource;
use lore_revision::state::node::hooks::log::Record;

use crate::harness::catalogue::call::Call;
use crate::harness::catalogue::call::Outcome;
use crate::harness::catalogue::call::Spawn;
use crate::harness::catalogue::call::Spawns;
use crate::harness::catalogue::call::held_twice;
use crate::harness::log::Collector;

/// One resource a step touched, and whether it wrote it.
pub type Touch = (Resource, bool);

/// What a step touched.
pub type Footprint = BTreeSet<Touch>;

/// How long a run waits for a task that goes on outside its thread, with none parked, before
/// calling it blocked.
const OUTSIDE: Duration = Duration::from_secs(10);

/// What a cancel's footprint holds: it drops whatever the task held, so it depends on every step.
const CANCEL: Touch = (Resource::Named("cancel"), true);

/// Whether two steps commute: neither is a cancel, and no resource both touch with a write
/// between them.
pub fn independent(a: &Footprint, b: &Footprint) -> bool {
    !a.contains(&CANCEL)
        && !b.contains(&CANCEL)
        && a.iter().all(|(resource, write)| {
            b.iter()
                .all(|(other, other_write)| resource != other || (!write && !other_write))
        })
}

/// A scenario task's state as the controller sees it.
#[derive(Debug)]
enum TaskState {
    /// Running a step.
    Running,
    /// Parked at a point, resumed by the sender.
    Parked(Point, tokio::sync::oneshot::Sender<()>),
    /// Done.
    Finished,
    /// Dropped where it was parked.
    Cancelled,
}

/// The state the tasks and the controller share.
///
/// The first tasks are the scenario's operations, by index. A future an operation spawns through
/// `hooks::inherit` is a task of its own, added as it is spawned, and part of the operation that
/// spawned it: it is numbered the same way in every run that takes the same choices, as a run is
/// one thread stepped deterministically.
struct Shared {
    tasks: Vec<TaskState>,
    /// The operation each task is part of.
    op: Vec<usize>,
    /// What the running step touched.
    footprint: Footprint,
    /// Spawned tasks that ended in a panic, which the operation awaiting them may have taken for
    /// an error: each fails the run.
    panicked: Vec<usize>,
    /// Signalled whenever a task parks or finishes.
    changed: Arc<tokio::sync::Notify>,
}

impl Shared {
    /// `task`'s future ended: finished, or dropped where it was parked, as the futures an
    /// operation spawned are when the operation is dropped.
    fn ended(&mut self, task: usize) {
        match self.tasks[task] {
            TaskState::Running => self.tasks[task] = TaskState::Finished,
            TaskState::Parked(..) => self.tasks[task] = TaskState::Cancelled,
            TaskState::Finished | TaskState::Cancelled => {}
        }
    }
}

/// Marks a scenario task's own future ended when dropped.
struct Finished {
    shared: Arc<Mutex<Shared>>,
    task: usize,
}

impl Drop for Finished {
    fn drop(&mut self) {
        let mut shared = lock(&self.shared);
        shared.ended(self.task);
        let changed = shared.changed.clone();
        drop(shared);
        changed.notify_one();
    }
}

/// The scheduler installed on the run's thread.
struct Gate {
    shared: Arc<Mutex<Shared>>,
    /// How each task came to be: by whose spawn, in which call; nothing for an operation's own.
    spawns: Arc<Spawns>,
    /// Whether readers park at their own points ([`Exploration::reader_points`]).
    reader_points: bool,
    /// The run's log, where it records ([`Exploration::recording`]).
    log: Option<Arc<Collector>>,
}

impl hooks::Scheduler for Gate {
    fn reached(&self, point: Point) -> Option<Pin<Box<dyn Future<Output = ()> + Send>>> {
        if let Some(log) = self.log.as_ref().filter(|log| log.recording()) {
            log.reserve();
        }
        let Some(task) = hooks::task() else {
            // A future spawned with no `inherit` never parks, and its records name no operation.
            if let Some(log) = self.log.as_ref().filter(|log| log.recording()) {
                log.flag(format!("{point:?} reached with no task"));
                panic!("{point:?} reached with no task while the run records");
            }
            return None;
        };
        let (resume, resumed) = tokio::sync::oneshot::channel();
        let changed = {
            let mut shared = lock(&self.shared);
            if matches!(shared.tasks[task], TaskState::Parked(..)) {
                None
            } else {
                shared.tasks[task] = TaskState::Parked(point, resume);
                Some(shared.changed.clone())
            }
        };
        // Asserted with the lock released, so the panic leaves it unpoisoned for the run's end.
        let changed =
            changed.unwrap_or_else(|| panic!("two futures of task {task} parked at once"));
        changed.notify_one();
        Some(Box::pin(async move {
            let _ = resumed.await;
        }))
    }

    fn touched(&self, resource: Resource, write: bool) {
        lock(&self.shared).footprint.insert((resource, write));
    }

    fn spawned(&self, task: usize) -> usize {
        let numbered = {
            let mut shared = lock(&self.shared);
            let op = shared.op[task];
            shared.tasks.push(TaskState::Running);
            shared.op.push(op);
            shared.tasks.len() - 1
        };
        // Told in the spawner's poll, where its call's scope is set.
        let spawned = self.spawns.spawned(task);
        assert_eq!(spawned, numbered, "a spawn numbered as its task");
        spawned
    }

    fn ended(&self, task: usize) {
        let changed = {
            let mut shared = lock(&self.shared);
            shared.ended(task);
            // Called as the spawned future unwinds, where it panicked.
            if std::thread::panicking() {
                shared.panicked.push(task);
            }
            shared.changed.clone()
        };
        changed.notify_one();
    }

    fn reader_points(&self) -> bool {
        self.reader_points
    }

    /// On the run's one thread a block's lock found held has a holder suspended at an await that
    /// is no scheduling point (one parked at a point fails `park`'s assertion first), which
    /// nothing can resume while the taker blocks: a deadlock, reported as the bug it is (C13).
    fn contended(&self, contention: &hooks::Contention) {
        panic!(
            "task {:?} found {:?} held, taking it {:?}: holders {:?}",
            contention.waiter, contention.block, contention.mode, contention.holders
        );
    }

    fn recording(&self) -> bool {
        self.log.as_ref().is_some_and(|log| log.recording())
    }

    fn effect(&self, record: Record) {
        if let Some(log) = &self.log {
            log.record(record);
        }
    }
}

thread_local! {
    /// The log of the run whose check runs on this thread ([`run_log`]).
    static RUN_LOG: RefCell<Vec<Record>> = const { RefCell::new(Vec::new()) };
    /// The calls of the run whose check runs on this thread ([`run_calls`]).
    static RUN_CALLS: RefCell<Vec<Call>> = const { RefCell::new(Vec::new()) };
    /// The operation of each task of the run whose check runs on this thread ([`run_ops`]).
    static RUN_OPS: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
    /// How each task of the run whose check runs on this thread came to be ([`run_spawns`]).
    static RUN_SPAWNS: RefCell<Vec<Spawn>> = const { RefCell::new(Vec::new()) };
    /// Whether the run whose check runs on this thread dropped an operation ([`run_cancelled`]).
    static RUN_CANCELLED: Cell<bool> = const { Cell::new(false) };
    /// Whether it dropped a task outside every call ([`run_dropped_outside_calls`]).
    static RUN_DROPPED_OUTSIDE: Cell<bool> = const { Cell::new(false) };
}

/// The log of the run being checked, in `(state, seq, sub)` order, taken: for a check of a
/// scenario explored with [`Exploration::recording`], empty otherwise.
pub fn run_log() -> Vec<Record> {
    RUN_LOG.with_borrow_mut(std::mem::take)
}

/// The calls of the run being checked, taken, by when they began: for a check of a scenario
/// explored with [`Exploration::recording`], empty otherwise.
pub fn run_calls() -> Vec<Call> {
    RUN_CALLS.with_borrow_mut(std::mem::take)
}

/// The operation each task of the run being checked was part of, by task: its operations first,
/// then the futures they spawned through `hooks::inherit`, each its spawner's operation's.
pub fn run_ops() -> Vec<usize> {
    RUN_OPS.with_borrow_mut(std::mem::take)
}

/// How each task of the run being checked came to be, nothing for an operation's own: the
/// futures an operation spawns through `hooks::inherit`, each with its spawner and the call open
/// on it as it spawned, so a call holds the records of what it spawned ([`Call::holds`]).
pub fn run_spawns() -> Vec<Spawn> {
    RUN_SPAWNS.with_borrow_mut(std::mem::take)
}

/// Whether the run being checked dropped one of its operations where it parked: a change it left
/// part way may excuse what its completion would have restored.
pub fn run_cancelled() -> bool {
    RUN_CANCELLED.get()
}

/// Whether the run being checked dropped a task where it parked outside every one of its calls
/// ([`dropped_outside_calls`]): what it left part way no call's records tell, which may excuse
/// closure on the whole State.
pub fn run_dropped_outside_calls() -> bool {
    RUN_DROPPED_OUTSIDE.get()
}

/// Whether one of the `dropped` tasks, each dropped where it parked, was part of an operation the
/// run `cancelled` and parked outside every one of `calls`: the operation's own task with no call
/// dropped with it, or a future it spawned outside every call, as `spawns` tell. A future spawned
/// in a call is that call's, whose records, on its State, tell what it left, whatever it answered;
/// one dropped with no operation cancelled, by a call's early return, is the dropping call's.
pub fn dropped_outside_calls(
    dropped: &[usize],
    cancelled: &[usize],
    calls: &[Call],
    spawns: &[Spawn],
) -> bool {
    // The operation `task` is part of, up its spawns, and whether a call holds it.
    let traced = |task: usize| {
        let (mut at, mut in_call) = (task, false);
        for _ in 0..=spawns.len() {
            let spawn = spawns.get(at)?;
            let Some(spawner) = spawn.spawner else {
                let own = at == task
                    && calls.iter().any(|call| {
                        call.task == Some(task) && matches!(call.outcome, Outcome::Dropped)
                    });
                return Some((at, in_call || own));
            };
            in_call |= spawn.call.is_some();
            at = spawner;
        }
        None
    };
    dropped.iter().any(|&task| {
        traced(task).is_none_or(|(operation, in_call)| cancelled.contains(&operation) && !in_call)
    })
}

/// The scheduler's state, whatever panicked while holding it: a panic in a task fails the run
/// at its end, which reads this state, so it must not fail it twice.
fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One operation of a scenario: given the scenario's context, a future run as a task.
pub type Op<C> = Box<dyn Fn(Arc<C>) -> Pin<Box<dyn Future<Output = ()> + Send>>>;

/// A scenario's setup: the context each run starts from.
pub type Setup<C> = Box<dyn Fn() -> Pin<Box<dyn Future<Output = C>>>>;

/// A scenario's check of a run's outcome.
pub type Check<C> = Box<dyn Fn(Arc<C>) -> Pin<Box<dyn Future<Output = ()>>>>;

/// A choice at a decision point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// Run the task's next step.
    Step(usize),
    /// Drop the operation, by index, where one of its tasks is parked. The futures it spawned
    /// go with it, but for those it detached.
    Cancel(usize),
}

impl Choice {
    fn task(self) -> usize {
        match self {
            Self::Step(task) | Self::Cancel(task) => task,
        }
    }
}

/// A run's trace, printed when a check fails.
#[derive(Clone, Debug, Default)]
pub struct Trace {
    pub steps: Vec<(Choice, Point)>,
}

impl std::fmt::Display for Trace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (choice, point) in &self.steps {
            match choice {
                Choice::Step(task) => writeln!(f, "  task {task} from {point:?}")?,
                Choice::Cancel(task) => writeln!(f, "  task {task} cancelled at {point:?}")?,
            }
        }
        Ok(())
    }
}

/// When a task that was resumed counts as blocked: waiting on something only a parked task can
/// give.
#[derive(Clone, Copy, Debug)]
pub enum Idle {
    /// After this many polls of the runtime with no task parking: for scenarios that only touch
    /// memory, where a task that can go on does so at once.
    Polls(usize),
    /// After this long with no task parking: for scenarios that do host I/O.
    Time(Duration),
}

/// The environment variable that sets an exploration's wall budget, in seconds
/// ([`Exploration::budget`]).
pub const BUDGET_VARIABLE: &str = "LORE_STATE_TESTS_BUDGET_SECONDS";

/// An exploration's wall budget where [`BUDGET_VARIABLE`] sets none: a guard against a hang, past
/// what any exploration a test runs takes, so no test's coverage rests on it.
const BUDGET: Duration = Duration::from_secs(3600);

/// The wall budget [`BUDGET_VARIABLE`] sets, or [`BUDGET`].
fn budget() -> Duration {
    std::env::var(BUDGET_VARIABLE)
        .ok()
        .and_then(|seconds| seconds.parse().ok())
        .map_or(BUDGET, Duration::from_secs)
}

/// How a scenario is explored.
#[derive(Clone, Copy, Debug)]
pub struct Exploration {
    /// Prune reorderings of independent steps; off where steps touch state no hook records.
    pub sleep_sets: bool,
    /// Also try dropping each cancellable task at each point it parks at that a future can be
    /// dropped at ([`Point::droppable`]).
    pub cancellation: bool,
    /// At most this many runs: the budget a test's completeness rests on.
    pub max_runs: usize,
    /// Stop exploring after this long, against a hang; the runs done are reported. A guard only,
    /// which an exploration that finishes never reaches ([`BUDGET_VARIABLE`]).
    pub budget: Duration,
    pub idle: Idle,
    /// At most this many preemptions per run: switches away from a task that could go on.
    /// Where [`Self::deepening`], exploration runs every bound from 0 up to this one in turn, so
    /// the schedules with the fewest preemptions, where most interleaving bugs show, come first,
    /// and stops at the first bound no schedule reached: that exploration was unbounded.
    /// Otherwise it runs this bound alone: every schedule the last of those passes runs, none of
    /// those within a lower bound run again.
    pub preemptions: usize,
    /// Whether exploration deepens its preemption bound from 0 ([`Self::preemptions`]): a
    /// scenario that fails then fails on a schedule with the fewest preemptions, at the cost of
    /// running every schedule within each bound again at each higher one.
    pub deepening: bool,
    /// Whether readers park at the points inside their walks of chains and paths
    /// (`hooks::reached_reader`), which multiply the interleavings of every reader.
    pub reader_points: bool,
    /// Whether each run records the log from the end of its setup, for its check to read
    /// ([`run_log`]); a record or a point with no task then fails the run.
    pub recording: bool,
}

impl Default for Exploration {
    fn default() -> Self {
        Self {
            sleep_sets: true,
            cancellation: false,
            max_runs: 200_000,
            budget: budget(),
            idle: Idle::Polls(256),
            preemptions: 16,
            deepening: true,
            reader_points: false,
            recording: false,
        }
    }
}

/// What an exploration covered.
#[derive(Clone, Copy, Debug, Default)]
pub struct Covered {
    pub runs: usize,
    /// The highest preemption bound whose every interleaving was run, or pruned as equivalent
    /// to one run, within the budget; with sleep sets on, only a bound that cut no schedule off.
    pub bound: Option<usize>,
    /// Whether exploration ran every interleaving, or pruned it as equivalent to one run: at a
    /// bound no schedule reached. Sleep sets are sound only so: under a bound that cuts a
    /// schedule off, a reordering they prune may be the one the bound kept from running.
    pub complete: bool,
    /// The most decision points one run took.
    pub deepest: usize,
    /// Whether exploration stopped at its time budget, against a hang, rather than at its run
    /// budget or its end.
    pub timed_out: bool,
}

/// A task parked at a decision point: the task, the operation it is part of, and where.
type Parked = (usize, usize, Point);

/// A node of the search: one decision point on the current path.
#[derive(Clone, Debug, Default)]
struct Node {
    /// The tasks parked here.
    enabled: Vec<Parked>,
    /// The choices explored from here, each with its step's footprint.
    done: Vec<(Choice, Footprint)>,
    /// Tasks asleep here, each with the footprint of the step it would take.
    sleep: Vec<(usize, Footprint)>,
    /// The choice the current path takes from here.
    chosen: Option<Choice>,
    /// The task that took the step before this node, and the preemptions before it.
    previous: Option<usize>,
    preempted: usize,
    /// Operations dropped on the path to this node, which a future they detached may outlive:
    /// none is dropped twice.
    cancelled: Vec<usize>,
}

/// A scenario: a setup making a context, the operations run concurrently on it, and the check
/// run once every operation has finished or been cancelled.
pub struct Scenario<C> {
    pub name: &'static str,
    pub setup: Setup<C>,
    pub ops: Vec<Op<C>>,
    /// The operations that may be cancelled, by index.
    pub cancellable: Vec<usize>,
    /// Run once every operation has finished or been cancelled, with no scheduler installed.
    pub check: Check<C>,
}

impl<C: Send + Sync + 'static> Scenario<C> {
    /// Explores the scenario's interleavings as `exploration` says, running the check after
    /// each; a failed check panics with the trace that led to it. What it covered, and how long it
    /// took, goes to stderr.
    pub fn explore(&self, exploration: Exploration) -> Covered {
        let started = Instant::now();
        let covered = self.explored(exploration, started);
        eprintln!(
            "'{}': {covered:?} in {:.1} s",
            self.name,
            started.elapsed().as_secs_f64()
        );
        covered
    }

    /// [`Self::explore`]'s exploration, begun at `started`.
    fn explored(&self, exploration: Exploration, started: Instant) -> Covered {
        let mut covered = Covered::default();
        let lowest = if exploration.deepening {
            0
        } else {
            exploration.preemptions
        };
        for bound in lowest..=exploration.preemptions {
            let exploration = Exploration {
                preemptions: bound,
                ..exploration
            };
            let limited = Cell::new(false);
            let mut path: Vec<Node> = Vec::new();
            loop {
                self.run(&mut path, exploration, &limited);
                covered.runs += 1;
                covered.deepest = covered.deepest.max(path.len());
                if !self.backtrack(&mut path, exploration, &limited) {
                    if !exploration.sleep_sets || !limited.get() {
                        covered.bound = Some(bound);
                    }
                    break;
                }
                if covered.runs >= exploration.max_runs || started.elapsed() >= exploration.budget {
                    covered.timed_out = started.elapsed() >= exploration.budget;
                    return covered;
                }
            }
            if !limited.get() {
                covered.complete = true;
                return covered;
            }
        }
        covered
    }

    /// The choices a node still offers: parked tasks neither asleep nor explored from it, then
    /// cancellations of the cancellable ones. Sets `limited` where the preemption bound kept a
    /// task from them.
    fn candidates(
        &self,
        node: &Node,
        exploration: Exploration,
        limited: &Cell<bool>,
    ) -> Vec<Choice> {
        let explored = |choice: Choice| node.done.iter().any(|(done, _)| *done == choice);
        let asleep =
            |task: usize| exploration.sleep_sets && node.sleep.iter().any(|(t, _)| *t == task);
        // Switching away from the task that ran the last step, while it can go on, preempts it.
        let previous_enabled = node
            .previous
            .filter(|previous| node.enabled.iter().any(|(task, _, _)| task == previous));
        let allowed = |task: usize| {
            let allowed = previous_enabled.is_none_or(|previous| {
                task == previous || node.preempted < exploration.preemptions
            });
            if !allowed {
                limited.set(true);
            }
            allowed
        };
        let mut choices: Vec<Choice> = node
            .enabled
            .iter()
            .map(|(task, _, _)| Choice::Step(*task))
            .filter(|choice| !explored(*choice) && !asleep(choice.task()) && allowed(choice.task()))
            .collect();
        if exploration.cancellation {
            // A cancel drops every task of the operation where it is parked, so each must be
            // parked where a future can be dropped.
            let droppable = |op: usize| {
                node.enabled
                    .iter()
                    .filter(|(_, of, _)| *of == op)
                    .all(|(_, _, point)| point.droppable())
            };
            for &(_, op, point) in &node.enabled {
                let choice = Choice::Cancel(op);
                if self.cancellable.contains(&op)
                    && point != Point::Named("start")
                    && droppable(op)
                    && !node.cancelled.contains(&op)
                    && !explored(choice)
                    && !choices.contains(&choice)
                {
                    choices.push(choice);
                }
            }
        }
        choices
    }

    /// Moves the search to the next unexplored choice: drops exhausted nodes from the end of
    /// the path and changes the deepest choice left. False when every choice is explored.
    fn backtrack(
        &self,
        path: &mut Vec<Node>,
        exploration: Exploration,
        limited: &Cell<bool>,
    ) -> bool {
        while let Some(node) = path.last_mut() {
            node.chosen = None;
            let next = self
                .candidates(node, exploration, limited)
                .into_iter()
                .next();
            if let Some(next) = next {
                node.chosen = Some(next);
                return true;
            }
            path.pop();
        }
        false
    }

    /// One run: follows `path`'s choices, extends it with the first unexplored choice at each
    /// new decision point, and records what each step touched.
    fn run(&self, path: &mut Vec<Node>, exploration: Exploration, limited: &Cell<bool>) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the run's runtime starts");
        let execution = new_execution();
        // So the States the setup makes are named in footprints as in every other run.
        hooks::reset_state_ordinals();
        let context =
            Arc::new(runtime.block_on(
                lore_base::runtime::LORE_CONTEXT.scope(execution.clone(), (self.setup)()),
            ));
        let changed = Arc::new(tokio::sync::Notify::new());
        let shared = Arc::new(Mutex::new(Shared {
            tasks: (0..self.ops.len()).map(|_| TaskState::Running).collect(),
            op: (0..self.ops.len()).collect(),
            footprint: Footprint::new(),
            panicked: Vec::new(),
            changed: changed.clone(),
        }));
        let log = exploration.recording.then(|| {
            let log = Collector::new();
            log.set_recording(true);
            // The run's one thread, before its first section.
            log.reserve();
            log
        });
        let spawns = Spawns::new(self.ops.len());
        let spawned = spawns.install();
        let installed = hooks::install(Arc::new(Gate {
            shared: shared.clone(),
            spawns: spawns.clone(),
            reader_points: exploration.reader_points,
            log: log.clone(),
        }));
        let entered = log.as_ref().map(Collector::enter);
        let mut trace = Trace::default();
        let outcome = runtime.block_on(lore_base::runtime::LORE_CONTEXT.scope(execution, async {
            let mut handles = Vec::with_capacity(self.ops.len());
            for (task, op) in self.ops.iter().enumerate() {
                let op = op(context.clone());
                let shared = shared.clone();
                handles.push(lore_base::lore_spawn!(hooks::as_task(task, async move {
                    // Marks the task finished however it ends, a panic included, so a panic
                    // reads as one rather than as a task that never went on.
                    let _finished = Finished { shared, task };
                    hooks::reached(Point::Named("start")).await;
                    op.await;
                })));
            }
            let mut depth = 0;
            loop {
                settle(&shared, &changed, exploration.idle).await;
                let mut enabled = parked(&shared);
                if enabled.is_empty() {
                    // A task still running with none parked waits on something outside the
                    // run's thread, as a C API call's end waits for its event forwarder on the
                    // core runtime: wait for it in real time before calling it blocked.
                    settle(&shared, &changed, Idle::Time(OUTSIDE)).await;
                    enabled = parked(&shared);
                    if enabled.is_empty() {
                        break;
                    }
                }
                if depth == path.len() {
                    let sleep = path.last().map(child_sleep).unwrap_or_default();
                    let cancelled = path.last().map_or_else(Vec::new, |parent| {
                        let mut cancelled = parent.cancelled.clone();
                        if let Some(Choice::Cancel(task)) = parent.chosen {
                            cancelled.push(task);
                        }
                        cancelled
                    });
                    let (previous, preempted) = match path.last() {
                        None => (None, 0),
                        Some(parent) => {
                            let chosen = parent.chosen.map(Choice::task);
                            let switched = parent.previous.is_some_and(|previous| {
                                Some(previous) != chosen
                                    && parent.enabled.iter().any(|(task, _, _)| *task == previous)
                            });
                            (chosen, parent.preempted + usize::from(switched))
                        }
                    };
                    path.push(Node {
                        sleep,
                        previous,
                        preempted,
                        cancelled,
                        ..Node::default()
                    });
                }
                let node = &mut path[depth];
                node.enabled.clone_from(&enabled);
                let choice = if let Some(choice) = node.chosen {
                    choice
                } else {
                    let choice = self
                        .candidates(node, exploration, limited)
                        .into_iter()
                        .next()
                        .unwrap_or(Choice::Step(enabled[0].0));
                    node.chosen = Some(choice);
                    choice
                };
                // The task the choice acts on where it is parked: a dropped operation's own, or
                // else the first of its tasks parked.
                let parked = match choice {
                    Choice::Step(task) => enabled.iter().find(|(t, _, _)| *t == task),
                    Choice::Cancel(op) => enabled
                        .iter()
                        .find(|(t, _, _)| *t == op)
                        .or_else(|| enabled.iter().find(|(_, o, _)| *o == op)),
                };
                let Some(&(task, _, point)) = parked else {
                    return Err(format!("no task of the choice {choice:?} is parked"));
                };
                trace.steps.push((choice, point));
                lock(&shared).footprint.clear();
                match choice {
                    Choice::Step(_) => {
                        let resume = {
                            let mut shared = lock(&shared);
                            match std::mem::replace(&mut shared.tasks[task], TaskState::Running) {
                                TaskState::Parked(_, resume) => resume,
                                other => {
                                    return Err(format!("task {task} is not parked: {other:?}"));
                                }
                            }
                        };
                        let _ = resume.send(());
                    }
                    Choice::Cancel(op) => {
                        // The operation's own future is dropped where it parked, or where it
                        // awaits the task parked; the futures it spawned are dropped with it,
                        // each where it parked, but for those it detached.
                        if task == op {
                            lock(&shared).tasks[op] = TaskState::Cancelled;
                        }
                        handles[op].abort();
                        aborted(&shared, &handles[op]).await;
                    }
                }
                settle(&shared, &changed, exploration.idle).await;
                let mut footprint = lock(&shared).footprint.clone();
                if matches!(choice, Choice::Cancel(_)) {
                    // A cancel drops whatever the task held: dependent on every step.
                    footprint.insert(CANCEL);
                }
                let node = &mut path[depth];
                match node.done.iter_mut().find(|(done, _)| *done == choice) {
                    Some(entry) => entry.1 = footprint,
                    None => node.done.push((choice, footprint)),
                }
                depth += 1;
            }
            let blocked: Vec<usize> = {
                let shared = lock(&shared);
                (0..shared.tasks.len())
                    .filter(|&task| matches!(shared.tasks[task], TaskState::Running))
                    .collect()
            };
            if !blocked.is_empty() {
                for handle in &handles {
                    handle.abort();
                }
                return Err(format!("tasks {blocked:?} never went on: a deadlock"));
            }
            for (task, handle) in handles.into_iter().enumerate() {
                if let Err(error) = handle.await
                    && error.is_panic()
                {
                    let payload = error.into_panic();
                    let message = payload
                        .downcast_ref::<String>()
                        .map(String::as_str)
                        .or_else(|| payload.downcast_ref::<&str>().copied())
                        .unwrap_or("a panic");
                    return Err(format!("task {task} panicked: {message}"));
                }
            }
            let panicked = std::mem::take(&mut lock(&shared).panicked);
            if !panicked.is_empty() {
                return Err(format!("spawned tasks {panicked:?} panicked"));
            }
            Ok(())
        }));
        drop(entered);
        drop(installed);
        drop(spawned);
        path.truncate(depth_of(path));
        let no_task = log.as_ref().and_then(|log| log.no_task());
        if let Err(error) = outcome {
            let no_task = no_task.map_or_else(String::new, |what| format!(" ({what})"));
            panic!(
                "scenario '{}': {error}{no_task}, after:\n{trace}",
                self.name
            );
        }
        if let Some(what) = no_task {
            panic!("scenario '{}': {what}, after:\n{trace}", self.name);
        }
        let records = log.as_ref().map(|log| log.drain()).unwrap_or_default();
        let calls = log.as_ref().map(|log| log.take_calls()).unwrap_or_default();
        if let Some(what) = log.as_ref().and_then(|log| log.no_task()) {
            panic!("scenario '{}': {what}, after:\n{trace}", self.name);
        }
        let (ops, spawns, dropped) = {
            let shared = lock(&shared);
            let dropped: Vec<usize> = (shared.tasks.iter().enumerate())
                .filter(|(_, task)| matches!(task, TaskState::Cancelled))
                .map(|(task, _)| task)
                .collect();
            (shared.op.clone(), spawns.spawns(), dropped)
        };
        let cancelled: Vec<usize> = path
            .iter()
            .filter_map(|node| match node.chosen {
                Some(Choice::Cancel(task)) => Some(task),
                _ => None,
            })
            .collect();
        if let Some((record, first, second)) = held_twice(&records, &calls, &spawns) {
            panic!(
                "scenario '{}': {record:?} held by two calls, {first:?} and {second:?}, after:\n{trace}",
                self.name
            );
        }
        RUN_DROPPED_OUTSIDE.set(dropped_outside_calls(&dropped, &cancelled, &calls, &spawns));
        RUN_CANCELLED.set(!cancelled.is_empty());
        RUN_LOG.set(records);
        RUN_CALLS.set(calls);
        RUN_OPS.set(ops);
        RUN_SPAWNS.set(spawns);
        let check = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(
                lore_base::runtime::LORE_CONTEXT.scope(new_execution(), (self.check)(context)),
            );
        }));
        RUN_CANCELLED.set(false);
        RUN_DROPPED_OUTSIDE.set(false);
        if let Err(panic) = check {
            eprintln!("scenario '{}' failed after:\n{trace}", self.name);
            std::panic::resume_unwind(panic);
        }
    }
}

/// The nodes of a path that were reached, which a shorter run than the replayed one leaves
/// fewer of.
fn depth_of(path: &[Node]) -> usize {
    path.iter().take_while(|node| node.chosen.is_some()).count()
}

/// The tasks parked now, each with its operation, and where.
fn parked(shared: &Arc<Mutex<Shared>>) -> Vec<Parked> {
    let shared = lock(shared);
    shared
        .tasks
        .iter()
        .enumerate()
        .filter_map(|(task, state)| match state {
            TaskState::Parked(point, _) => Some((task, shared.op[task], *point)),
            _ => None,
        })
        .collect()
}

/// Waits until no task is running a step: each parked, finished or cancelled. A task waiting on
/// something only a parked task can give stays running; the wait gives up on it as `idle` says
/// and the run goes on without it.
async fn settle(shared: &Arc<Mutex<Shared>>, changed: &Arc<tokio::sync::Notify>, idle: Idle) {
    let mut polls = 0;
    let mut since = Instant::now();
    loop {
        let running = lock(shared)
            .tasks
            .iter()
            .any(|state| matches!(state, TaskState::Running));
        if !running {
            return;
        }
        match idle {
            Idle::Polls(limit) => {
                tokio::select! {
                    biased;
                    () = changed.notified() => polls = 0,
                    () = tokio::task::yield_now() => {
                        polls += 1;
                        if polls > limit {
                            return;
                        }
                    }
                }
            }
            Idle::Time(limit) => {
                tokio::select! {
                    biased;
                    () = changed.notified() => since = Instant::now(),
                    () = tokio::time::sleep(Duration::from_millis(1)) => {
                        if since.elapsed() > limit {
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// Waits for an aborted operation to be dropped, and for the futures it spawned into sets to be
/// dropped with it: each is dropped when the runtime next polls it, so the wait yields until the
/// operation's task is done and then until a yield ends no further task. Its detached futures
/// go on.
async fn aborted(shared: &Arc<Mutex<Shared>>, handle: &tokio::task::JoinHandle<()>) {
    while !handle.is_finished() {
        tokio::task::yield_now().await;
    }
    let ended = |shared: &Arc<Mutex<Shared>>| {
        lock(shared)
            .tasks
            .iter()
            .filter(|state| matches!(state, TaskState::Finished | TaskState::Cancelled))
            .count()
    };
    let mut before = ended(shared);
    loop {
        tokio::task::yield_now().await;
        let after = ended(shared);
        if after == before {
            return;
        }
        before = after;
    }
}

/// The sleep set of the node a step from `parent` leads to: the tasks asleep at `parent` or
/// explored from it before the chosen step, whose steps commute with the chosen one.
fn child_sleep(parent: &Node) -> Vec<(usize, Footprint)> {
    let Some(chosen) = parent.chosen else {
        return Vec::new();
    };
    let Some((_, taken)) = parent.done.iter().find(|(done, _)| *done == chosen) else {
        return Vec::new();
    };
    let mut sleep: Vec<(usize, Footprint)> = parent
        .sleep
        .iter()
        .filter(|(task, footprint)| *task != chosen.task() && independent(footprint, taken))
        .cloned()
        .collect();
    for (done, footprint) in &parent.done {
        if let Choice::Step(task) = *done
            && *done != chosen
            && task != chosen.task()
            && independent(footprint, taken)
            && !sleep.iter().any(|(t, _)| *t == task)
        {
            sleep.push((task, footprint.clone()));
        }
    }
    sleep
}

fn new_execution() -> Arc<lore_revision::interface::ExecutionContext> {
    crate::harness::fixtures::execution()
}
