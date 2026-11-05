// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Scheduling points of the node operations, for the deterministic and chaos schedulers of the
//! stress harness (`lore-revision/examples/state_stress`), which the deterministic scenarios share.
//! Built only with `state-hooks`.
//!
//! A scheduler is installed per thread, so the tests of one process, each on a `current_thread`
//! runtime of its own, never see each other's. Without one installed anywhere, a point costs one
//! relaxed load.

use std::cell::Cell;
use std::cell::RefCell;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;

pub mod log;

/// A place where a node operation holds no lock and may be suspended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Point {
    /// A name looked up, before the change that rests on it.
    Lookup,
    /// One step of a find by name through a chain.
    FindStep,
    /// A slot grabbed for a new node.
    Grabbed,
    /// A grabbed node initialized, not yet published.
    Initialized,
    /// Just before a node is published into its directory's chain.
    BeforePublish,
    /// One step of the walk to a node's predecessor, and once more between that walk and a
    /// detach's locked section.
    PredecessorStep,
    /// One step of the walk up checking a move does not enter its own subtree.
    SubtreeStep,
    /// Just before a move takes its locks.
    BeforeMoveLock,
    /// Just before a rewrite takes its locks.
    BeforeRewrite,
    /// Between a restore's read of its node's parent and its lock of the pair, which no await
    /// separates where the parent's block is in memory.
    RestoreLock,
    /// Block locks dropped, before the blocks they dirtied are registered.
    BeforeRegistration,
    /// One level of a mark's climb, served or unserved.
    ClimbStep,
    /// Before each level of a served mark's descent, no lock held.
    DescentStep,
    /// A served mark's climb found a block not in memory.
    NeedLoad,
    /// A change retried after a failed check.
    Retry,
    /// Between the phases of a clear.
    ClearPhase,
    /// Inside a serialize, between its block writes.
    Serialize,
    /// Inside a save's threading of the unused chain, the unused permit held, before the chain's
    /// head is pointed at the blocks threaded (or, on an empty chain, set to them).
    Threading,
    /// Inside the stage's directory update window (stage.rs).
    StageWindow,
    /// One child of a swap's private copy filled.
    SwapFill,
    /// Between the frees of an abandoned copy's nodes.
    Abandon,
    /// Between the steps of a discard: after its roots' detaches, between the runs of a chain a
    /// pending discard marks, each in one block, and before a C API delete marks a node of a
    /// level.
    Discard,
    /// One step of a children iterator or a `ChainWalk`; in a walk of a directory's children
    /// (`State::walk_children`), its going on to another block or starting again.
    IterStep,
    /// One step of a node's path walk.
    NodePath,
    /// A directory's chain log read.
    ListingLog,
    /// An absence checked against a chain log.
    AbsentCheck,
    /// A block about to load from the store, where a drain of the pending queue reads one not in
    /// memory.
    BlockLoad,
    /// A block about to be written to the store.
    BlockSave,
    /// A point a test names, outside the node operations.
    Named(&'static str),
}

impl Point {
    /// Whether a task parked here may be dropped: outside the scheduler an await stands at or
    /// beside the point. A point between two steps no await separates outside it, where another
    /// thread may run meanwhile but no future can be dropped, is not: `BeforeRegistration`,
    /// between a block's dirty mark and its registration, `Threading`, between a save's
    /// threading of blocks and its write of the chain's head, and `RestoreLock`.
    pub fn droppable(self) -> bool {
        !matches!(
            self,
            Point::BeforeRegistration | Point::Threading | Point::RestoreLock
        )
    }
}

/// A lock or shared value one step of a task reads or writes, so a scheduler can tell which
/// steps commute. Each is named the same way in every run of a scenario, which builds its State
/// afresh, so footprints recorded in different runs compare: a State by its creation ordinal on
/// its thread, which a deterministic run restarts before its setup ([`reset_state_ordinals`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Resource {
    /// A node block's lock: its State's ordinal, and the index the State installed it at.
    Block(usize, usize),
    /// A file-metadata block's lock: its State's ordinal, and its index.
    MetadataBlock(usize, usize),
    /// A State's runtime lock, by the State's ordinal.
    Runtime(usize),
    /// A State's data lock, by the State's ordinal.
    Data(usize),
    /// A State's moves permit, by the State's ordinal.
    Permit(usize),
    /// A State's unused-slot permit, by the State's ordinal.
    UnusedPermit(usize),
    /// Anything else a test names.
    Named(&'static str),
}

/// A window between two steps of an operation that no await separates, where another thread can
/// still run: a scheduler may stall the thread there to widen it ([`Scheduler::window`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Window {
    /// `node_rename`'s, between its lookup of the parent's block and its section.
    Rename,
    /// `find_subnode_resident`'s, before each node its walk reads, which no await separates.
    ResidentFind,
}

/// What an operation waits on that another task, or the store, gives it
/// ([`Scheduler::waiting`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Wait {
    /// A State's moves permit.
    MovesPermit,
    /// A State's unused-slot permit.
    UnusedPermit,
    /// A node block's load from the store: the tree's read, the block list's lock and read, the
    /// loading permit, the block's read.
    BlockLoad,
    /// A node block's name table's load from the store.
    NameTable,
    /// A file metadata block's load from the store.
    FileMetadata,
}

/// How a holder holds a block's lock, as a lock probe names it ([`Contention`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    Read,
    Write,
    /// A save's upgradable read, which shuts out writers and other upgradable reads.
    Upgradable,
}

/// A holder of a block's lock: the scheduler task holding it, `None` outside a task, and how.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Holder {
    pub task: Option<usize>,
    pub mode: Mode,
}

/// The most holders of one block's lock a probe names: a writer, or an upgradable reader, and
/// this many less one readers; readers past those go unnamed.
pub const HOLDER_SLOTS: usize = 5;

/// A block's lock found held where a task went to take it: by a holder, or, for a read, by a
/// writer waiting for the readers to drain.
#[derive(Clone, Copy, Debug)]
pub struct Contention {
    /// The block.
    pub block: Resource,
    /// The task taking the lock, `None` outside a task.
    pub waiter: Option<usize>,
    /// How it takes it.
    pub mode: Mode,
    /// The holders the probe named, read right after the failed try: none where the lock was
    /// taken back meanwhile, held by readers past the named ones, or handed over in a fair
    /// unlock before its new owner ran.
    pub holders: [Option<Holder>; HOLDER_SLOTS],
}

/// What a scheduler is told. Every call comes from a thread it is installed on, except a wait's
/// end ([`Waiting`]) and a spawned future's end ([`Self::ended`]): each comes from the thread it
/// ends on, and goes to the scheduler that heard it begin.
pub trait Scheduler: Send + Sync {
    /// The current task reached `point`. The future returned, if any, is awaited there before
    /// the task goes on.
    fn reached(&self, point: Point) -> Option<Pin<Box<dyn Future<Output = ()> + Send>>>;

    /// The running step read (`write` false) or wrote `resource`.
    fn touched(&self, resource: Resource, write: bool);

    /// Task `task` spawned a future that runs on its behalf ([`inherit`]): the task that future
    /// runs as, `task` itself unless the scheduler tells the two apart.
    fn spawned(&self, task: usize) -> usize {
        task
    }

    /// A future spawned through [`inherit`] to run as `task` ended, finished or dropped. Told as
    /// the future unwinds where it panicked, so [`std::thread::panicking`] says so.
    fn ended(&self, _task: usize) {}

    /// Whether walks park at their own points ([`reached_reader`]) as well: the points inside a
    /// walk of a chain or a path, a reader's or a writer's.
    fn reader_points(&self) -> bool {
        false
    }

    /// The thread is in `window`; a scheduler may stall it there. No lock is held.
    fn window(&self, _window: Window) {}

    /// The current task begins waiting on `wait` where `begins`, or has stopped waiting.
    fn waiting(&self, _wait: Wait, _begins: bool) {}

    /// A probe found a block's lock held: told before the task blocks on it, holding no lock of
    /// that block.
    fn contended(&self, _contention: &Contention) {}

    /// Whether the scheduler takes the log's records now ([`log::record`]); the sections number
    /// none while it does not.
    fn recording(&self) -> bool {
        false
    }

    /// A record of the log, from the thread it was made on.
    fn effect(&self, _record: log::Record) {}
}

/// The scheduler's task a future runs as, with the count of owned block holds it has open. A
/// future spawned through [`inherit`] counts its own: on a multi-thread runtime a sibling may hold
/// a name lock for a moment, with no await, while another parks.
struct TaskId {
    id: usize,
    holds: Arc<AtomicUsize>,
}

impl TaskId {
    fn new(id: usize) -> Self {
        Self {
            id,
            holds: Arc::new(AtomicUsize::new(0)),
        }
    }
}

tokio::task_local! {
    /// The scheduler's task a future runs as: set by the scheduler, and carried into the futures
    /// a task spawns through [`inherit`].
    static TASK: TaskId;
}

/// The scheduler's task the running future is part of, if any.
pub fn task() -> Option<usize> {
    TASK.try_with(|task| task.id).ok()
}

/// The count of owned block holds the running task has open, for a hold to count itself in, so
/// [`reached`] can tell the task parks holding none (C13). `None` outside a task.
pub(crate) fn task_holds() -> Option<Arc<AtomicUsize>> {
    TASK.try_with(|task| task.holds.clone()).ok()
}

/// `future`, run as the scheduler's task `task`. A plain function returning the scope, so the
/// future is held once, in it.
pub fn as_task<F: Future>(task: usize, future: F) -> impl Future<Output = F::Output> {
    TASK.scope(TaskId::new(task), IdleWhenSuspended { future })
}

/// A task's future, asserting at each suspension that it left no section of the log open or
/// labelled: one left so would carry its label or `seq` into the next task the thread polls.
#[pin_project::pin_project]
struct IdleWhenSuspended<F> {
    #[pin]
    future: F,
}

impl<F: Future> Future for IdleWhenSuspended<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<F::Output> {
        let poll = self.project().future.poll(context);
        assert!(
            poll.is_ready() || log::idle(),
            "a task suspended inside a labelled or open section of the log"
        );
        poll
    }
}

/// `future`, for a task spawned on behalf of the running one: it parks at the scheduling points
/// as the running task's own steps do, as the task the scheduler names for it, and the scheduler
/// hears when it ends. Outside a scheduler's task, `future` as it is.
pub fn inherit<F: Future>(future: F) -> impl Future<Output = F::Output> {
    let Some(task) = task() else {
        return Inherited::Plain { future };
    };
    match current() {
        Some(scheduler) => {
            let task = scheduler.spawned(task);
            Inherited::Scoped {
                future: TASK.scope(TaskId::new(task), IdleWhenSuspended { future }),
                spawned: Some(Spawned { scheduler, task }),
            }
        }
        // Spawned from a thread with no scheduler, which records nothing: where the future runs
        // on one that has, its records still name the task.
        None => Inherited::Scoped {
            future: TASK.scope(TaskId::new(task), IdleWhenSuspended { future }),
            spawned: None,
        },
    }
}

/// [`inherit`]'s future, holding the spawned one once, in its scope or alone.
#[pin_project::pin_project(project = InheritedProjection)]
enum Inherited<F: Future> {
    /// Run as the task the scheduler named; the future is declared first, so where the whole is
    /// dropped unfinished it drops before the scheduler hears the task ended.
    Scoped {
        #[pin]
        future: tokio::task::futures::TaskLocalFuture<TaskId, IdleWhenSuspended<F>>,
        /// Taken, telling the scheduler, once the future ends.
        spawned: Option<Spawned>,
    },
    Plain {
        #[pin]
        future: F,
    },
}

impl<F: Future> Future for Inherited<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<F::Output> {
        match self.project() {
            InheritedProjection::Scoped { future, spawned } => {
                let poll = future.poll(context);
                if poll.is_ready() {
                    drop(spawned.take());
                }
                poll
            }
            InheritedProjection::Plain { future } => future.poll(context),
        }
    }
}

/// Tells the scheduler a spawned future ended when dropped, however it ends.
struct Spawned {
    scheduler: Arc<dyn Scheduler>,
    task: usize,
}

impl Drop for Spawned {
    fn drop(&mut self) {
        self.scheduler.ended(self.task);
    }
}

/// Threads with a scheduler installed, so the uninstalled path is one relaxed load.
static INSTALLED: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static SCHEDULER: RefCell<Option<Arc<dyn Scheduler>>> = const { RefCell::new(None) };
    /// The creation ordinal the next State made on this thread takes.
    static NEXT_ORDINAL: Cell<usize> = const { Cell::new(0) };
}

/// Installs `scheduler` on the calling thread until the guard drops. Tops the thread's `Word`
/// snapshot buffer up first, so its first section, before any point, allocates nothing.
pub fn install(scheduler: Arc<dyn Scheduler>) -> Installed {
    crate::node::reserve_words();
    SCHEDULER.with_borrow_mut(|slot| {
        assert!(slot.is_none(), "one scheduler per thread");
        *slot = Some(scheduler);
    });
    INSTALLED.fetch_add(1, Ordering::Relaxed);
    Installed {
        _thread: PhantomData,
    }
}

/// A scheduler installed on this thread; dropping it uninstalls it. Neither `Send` nor `Sync`, so
/// it drops on the thread that installed it.
pub struct Installed {
    _thread: PhantomData<*const ()>,
}

impl Drop for Installed {
    fn drop(&mut self) {
        let scheduler = SCHEDULER.with_borrow_mut(Option::take);
        INSTALLED.fetch_sub(1, Ordering::Relaxed);
        // Dropped outside the borrow: a scheduler's drop may reach the hooks itself.
        drop(scheduler);
    }
}

fn current() -> Option<Arc<dyn Scheduler>> {
    if INSTALLED.load(Ordering::Relaxed) == 0 {
        return None;
    }
    SCHEDULER.with_borrow(Clone::clone)
}

/// `f` of the thread's scheduler, borrowed: no reference count is touched, so a touch or a record
/// inside a section makes no write a thread on another worker shares.
#[inline]
pub(crate) fn with_current<R>(f: impl FnOnce(&dyn Scheduler) -> R) -> Option<R> {
    if INSTALLED.load(Ordering::Relaxed) == 0 {
        return None;
    }
    SCHEDULER.with_borrow(|slot| slot.as_deref().map(f))
}

/// A scheduling point: suspends the current task there if the thread's scheduler says so.
/// Where a scheduler is installed on the thread, asserts the thread holds no block guard and its
/// task no owned block hold, where a parked task would keep every other task from the block (C13).
pub async fn reached(point: Point) {
    let Some(scheduler) = current() else {
        return;
    };
    park(scheduler, point).await;
}

/// A scheduling point inside a walk of a chain or a path, where the walker holds no guard: a
/// reader's, or a writer's that walks as readers do (a reattach marking the subtree it moved, a
/// load of a directory's children). [`reached`] where the thread's scheduler asks for these points
/// ([`Scheduler::reader_points`]), nothing otherwise.
pub async fn reached_reader(point: Point) {
    let Some(scheduler) = current() else {
        return;
    };
    if scheduler.reader_points() {
        park(scheduler, point).await;
    }
}

async fn park(scheduler: Arc<dyn Scheduler>, point: Point) {
    let guards = crate::node::block_guards_held();
    assert!(
        guards == 0,
        "a task reached {point:?} holding {guards} node block guards (C13)"
    );
    if let Ok(holds) = TASK.try_with(|task| task.holds.load(Ordering::Relaxed)) {
        assert!(
            holds == 0,
            "a task reached {point:?} holding {holds} owned node block holds (C13)"
        );
    }
    assert!(
        log::idle(),
        "a task reached {point:?} inside a labelled or open section of the log"
    );
    // Outside every section: the snapshots' buffer grows here, not under a block's lock.
    if scheduler.recording() {
        crate::node::reserve_words();
    }
    let park = scheduler.reached(point);
    drop(scheduler);
    if let Some(park) = park {
        park.await;
    }
}

/// Records that the running step read or wrote `resource`.
#[inline]
pub fn touched(resource: Resource, write: bool) {
    with_current(|scheduler| scheduler.touched(resource, write));
}

/// The thread is in `window`, where the scheduler may stall it; it holds no block guard there.
pub fn window(window: Window) {
    // Borrowed, no count touched: a walk reaches its window once per node.
    with_current(|scheduler| {
        let guards = crate::node::block_guards_held();
        assert!(
            guards == 0,
            "a thread in the window {window:?} holds {guards} node block guards"
        );
        scheduler.window(window);
    });
}

/// The node block guards the calling thread holds; owned guards are not among them.
pub fn guards_held() -> usize {
    crate::node::block_guards_held()
}

/// The current task begins waiting on `wait`; the guard tells the scheduler that held it when
/// the wait ends, however it ends and on whatever thread.
pub fn waiting(wait: Wait) -> Waiting {
    let scheduler = current();
    if let Some(scheduler) = &scheduler {
        scheduler.waiting(wait, true);
    }
    Waiting { scheduler, wait }
}

/// A wait in progress ([`waiting`]).
pub struct Waiting {
    scheduler: Option<Arc<dyn Scheduler>>,
    wait: Wait,
}

impl Drop for Waiting {
    fn drop(&mut self) {
        if let Some(scheduler) = &self.scheduler {
            scheduler.waiting(self.wait, false);
        }
    }
}

/// Tells the thread's scheduler a probe found a block's lock held.
pub fn contended(contention: &Contention) {
    with_current(|scheduler| scheduler.contended(contention));
}

/// The creation ordinal of the next State made on this thread, which names it in footprints.
pub(crate) fn next_state_ordinal() -> usize {
    NEXT_ORDINAL.with(|next| {
        let ordinal = next.get();
        next.set(ordinal + 1);
        ordinal
    })
}

/// Restarts this thread's State ordinals, so the States a run's setup makes are named as in
/// every other run.
pub fn reset_state_ordinals() {
    NEXT_ORDINAL.with(|next| next.set(0));
}

/// The id the next State made in the process takes.
static NEXT_STATE_ID: AtomicU64 = AtomicU64::new(0);

/// A value on a cache line of its own (two, for the adjacent-line prefetcher).
#[repr(align(128))]
struct Padded<T>(T);

/// What names a State to the scheduler, and what a test lowers or reads of it, shared with the
/// blocks the State installs.
pub struct StateHooks {
    /// The State's creation ordinal on its thread.
    ordinal: usize,
    /// The State's process-unique id, which names it in the log.
    id: u64,
    /// The next `seq` a section of the State takes ([`log`]), on a cache line of its own: every
    /// section writes it, under its locks, and the fields beside it are read on every lock take
    /// and record.
    seq: Padded<AtomicU64>,
    /// The instance the next block installed in the State takes.
    instances: AtomicU64,
    /// The most attempts a change to a chain, and the most restarts a walk of one, makes before
    /// `SlowDown`; 0 for the product's own bounds.
    retry_bound: AtomicU32,
    /// The node blocks installed in the State and not dropped since.
    live_blocks: AtomicUsize,
}

impl StateHooks {
    /// The hooks of a State made now on this thread.
    #[lore_macro::test_pub]
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            ordinal: next_state_ordinal(),
            id: NEXT_STATE_ID.fetch_add(1, Ordering::Relaxed),
            seq: Padded(AtomicU64::new(0)),
            // From 1: 0 names no block, as `BlockLocks::hook_instance` answers for one not held.
            instances: AtomicU64::new(1),
            retry_bound: AtomicU32::new(0),
            live_blocks: AtomicUsize::new(0),
        })
    }

    /// The State's creation ordinal.
    pub fn ordinal(&self) -> usize {
        self.ordinal
    }

    /// The State's process-unique id.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The `seq` the next section takes: loaded just before an operation and just after its end,
    /// it bounds the operation's interval.
    pub fn seq_now(&self) -> u64 {
        self.seq.0.load(Ordering::Relaxed)
    }

    /// A `seq` for a section, taken once its locks are; ordered with every other section of a
    /// block it shares by that block's lock.
    pub(crate) fn next_seq(&self) -> u64 {
        self.seq.0.fetch_add(1, Ordering::Relaxed)
    }

    /// The instance of a block installed now, which tells its records from those of an evicted
    /// one at its index.
    pub(crate) fn next_instance(&self) -> u64 {
        self.instances.fetch_add(1, Ordering::Relaxed)
    }

    /// The bound a test set, if any.
    pub(crate) fn retry_bound(&self) -> Option<u32> {
        match self.retry_bound.load(Ordering::Relaxed) {
            0 => None,
            bound => Some(bound),
        }
    }

    /// Sets the bound, or, with `None`, restores the product's.
    pub(crate) fn set_retry_bound(&self, bound: Option<u32>) {
        let bound = bound.map_or(0, |bound| {
            assert!(bound > 0, "a retry bound of at least one attempt");
            bound
        });
        self.retry_bound.store(bound, Ordering::Relaxed);
    }

    /// The node blocks installed in the State and not dropped since.
    pub fn live_blocks(&self) -> usize {
        self.live_blocks.load(Ordering::Relaxed)
    }

    /// A block was installed in the State.
    pub(crate) fn block_installed(&self) {
        self.live_blocks.fetch_add(1, Ordering::Relaxed);
    }

    /// A block installed in the State was dropped.
    pub(crate) fn block_dropped(&self) {
        self.live_blocks.fetch_sub(1, Ordering::Relaxed);
    }
}
