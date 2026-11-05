// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The log's collector: the records a recording scheduler is told (`hooks::log`), kept in a buffer
//! per thread, so threads recording at once share no lock, and merged in `(state, seq, sub)` order
//! when read. Each buffer sits behind a mutex of its own, which only a drain contends; taking it
//! still orders memory on the recording thread, inside the section that records.
//!
//! A record made with no task sets a flag rather than panic, as a panic inside a section would
//! leave it half-applied; the driver fails the run on it.
//!
//! A record is pushed inside the section that made it, so a push must not allocate: a thread's
//! buffer is topped up outside every section, as recording turns on and at each of a scheduler's
//! points while it records ([`Collector::reserve`]).
//! Recording is turned on or off, and the records drained, only at quiescence: a section running
//! across a switch logs part of itself, and a record pushed during a drain lands in the next.
//!
//! It keeps the run's calls too ([`crate::harness::catalogue::call`]), in the same buffers: the
//! catalogue's operations keep each call as it ends, outside every section, on threads that
//! entered the collector ([`Collector::enter`]).

use std::cell::RefCell;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lore_revision::node::BLOCK_NODE_COUNT;
use lore_revision::state::node::hooks;
use lore_revision::state::node::hooks::Point;
use lore_revision::state::node::hooks::Resource;
use lore_revision::state::node::hooks::log::Record;

use crate::harness::catalogue::call::Call;
use crate::harness::catalogue::call::overlapping;

/// One thread's records and calls for one collector, on cache lines no other thread's buffer
/// shares, as a record's push inside a section takes its lock.
#[derive(Default)]
#[repr(align(128))]
struct Thread {
    records: Vec<Record>,
    calls: Vec<Call>,
    /// Calls opened here less calls kept here: a call opened on one worker may end on another,
    /// so only the sum over every thread counts the calls open.
    open: i64,
}

/// One thread's share of a collector.
type Buffer = Arc<Mutex<Thread>>;

static NEXT_COLLECTOR: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// The collector this thread recorded for last, and its buffer there: a thread records for
    /// one collector at a time, a chaos worker for its runtime's, a run's thread for the run's.
    static BUFFER: RefCell<Option<(u64, Buffer)>> = const { RefCell::new(None) };
    /// The collector of the run this thread runs, which its calls go to ([`Collector::enter`]).
    static ENTERED: RefCell<Option<Arc<Collector>>> = const { RefCell::new(None) };
}

/// What every record and call reads, on a cache line of its own, which the writes beside it (a
/// registered buffer, a fault) never share.
#[repr(align(128))]
struct Hot {
    id: u64,
    recording: AtomicBool,
}

/// The records and calls of one run, and whether it takes them now.
pub struct Collector {
    hot: Hot,
    /// Every thread's buffer, registered at its first record or call.
    buffers: Mutex<Vec<Buffer>>,
    /// The first fault: a record made with no task, a point reached with none, a call made with
    /// none, left open, or overlapping another of its task's.
    no_task: Mutex<Option<String>>,
    /// Records pushed into a full buffer: an allocation where the push may be inside a section.
    grown: AtomicU64,
}

impl Collector {
    /// A collector, not yet recording.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            hot: Hot {
                id: NEXT_COLLECTOR.fetch_add(1, Ordering::Relaxed),
                recording: AtomicBool::new(false),
            },
            buffers: Mutex::new(Vec::new()),
            no_task: Mutex::new(None),
            grown: AtomicU64::new(0),
        })
    }

    /// Makes this the collector the calling thread's calls go to, until the guard drops: for a
    /// run's thread while the run lasts, for a chaos worker while it lives.
    pub fn enter(self: &Arc<Self>) -> Entered {
        ENTERED.with_borrow_mut(|slot| {
            assert!(slot.is_none(), "one run's collector entered per thread");
            *slot = Some(self.clone());
        });
        Entered {
            _thread: PhantomData,
        }
    }

    /// The collector the calling thread entered, where it records.
    pub(crate) fn entered() -> Option<Arc<Self>> {
        ENTERED.with_borrow(|slot| slot.as_ref().filter(|log| log.recording()).cloned())
    }

    /// A call began, to be kept by [`Self::call_closed`].
    pub(crate) fn call_opened(&self) {
        self.with_buffer(|buffer| buffer.open += 1);
    }

    /// Keeps `call`, ended or dropped, in this thread's buffer.
    pub(crate) fn call_closed(&self, call: Call) {
        self.with_buffer(|buffer| {
            buffer.calls.push(call);
            buffer.open -= 1;
        });
    }

    /// The run's calls, taken at quiescence, by State, then when they began. A call still open,
    /// or two of one task's on one State overlapping but for one made in the other's window
    /// ([`overlapping`]), are faults.
    pub fn take_calls(&self) -> Vec<Call> {
        let mut calls: Vec<Call> = Vec::new();
        let mut open = 0;
        for buffer in guard(&self.buffers).iter() {
            let mut buffer = guard(buffer);
            open += buffer.open;
            calls.append(&mut buffer.calls);
        }
        if open > 0 {
            self.flag(format!("{open} calls still open"));
        }
        calls.sort_by_key(|call| (call.state, call.begin, call.task));
        if let Some((outer, inner)) = overlapping(&calls) {
            self.flag(format!(
                "overlapping calls of one task: {outer:?} and {inner:?}"
            ));
        }
        calls
    }

    /// Turns recording on, once a setup ends, or off, for a check's reads. Turning it on tops up
    /// every buffer registered so far, at quiescence, as no thread records yet.
    pub fn set_recording(&self, on: bool) {
        // Under the list's lock, so a thread registering meanwhile either is topped up here or
        // reads the flag set and tops itself up.
        let buffers = guard(&self.buffers);
        self.hot.recording.store(on, Ordering::Relaxed);
        if on {
            for buffer in buffers.iter() {
                top_up(&mut guard(buffer).records);
            }
        }
    }

    pub fn recording(&self) -> bool {
        self.hot.recording.load(Ordering::Relaxed)
    }

    /// Keeps `record`, in this thread's buffer.
    pub fn record(&self, record: Record) {
        if record.task.is_none() {
            self.flag(format!(
                "a record with no task: {:?} in {:?}",
                record.effect, record.section
            ));
        }
        self.with_buffer(|buffer| {
            let records = &mut buffer.records;
            if records.len() == records.capacity() {
                self.grown.fetch_add(1, Ordering::Relaxed);
            }
            records.push(record);
        });
    }

    /// Registers this thread's buffer for [`Self::set_recording`] to top up, topped up at once
    /// where recording is on already: at a worker's start, so a worker's first record allocates
    /// nothing whether or not a point came first, and a run that never records reserves no room
    /// for records.
    pub fn register(&self) {
        self.with_buffer(|buffer| {
            if self.recording() {
                top_up(&mut buffer.records);
            }
        });
    }

    /// Tops this thread's buffer up, registering it at the thread's first call, where the thread
    /// is outside every section: at each scheduling point while recording.
    pub fn reserve(&self) {
        self.with_buffer(|buffer| top_up(&mut buffer.records));
    }

    /// How many records were pushed into a full buffer, each an allocation that may have been
    /// inside a section.
    pub fn grown(&self) -> u64 {
        self.grown.load(Ordering::Relaxed)
    }

    /// `f` of this thread's buffer, registered at the thread's first use.
    fn with_buffer<R>(&self, f: impl FnOnce(&mut Thread) -> R) -> R {
        BUFFER.with_borrow_mut(|slot| {
            if !matches!(slot, Some((id, _)) if *id == self.hot.id) {
                let buffer = Buffer::default();
                guard(&self.buffers).push(buffer.clone());
                *slot = Some((self.hot.id, buffer));
            }
            let Some((_, buffer)) = slot.as_ref() else {
                unreachable!("the slot holds this collector's buffer");
            };
            f(&mut guard(buffer))
        })
    }

    /// Notes a fault the driver fails the run on; the first is kept.
    pub fn flag(&self, what: String) {
        guard(&self.no_task).get_or_insert(what);
    }

    /// The first fault, if any: a record or a point with no task, a call left open or
    /// overlapping another of its task's.
    pub fn no_task(&self) -> Option<String> {
        guard(&self.no_task).clone()
    }

    /// Every record so far, taken out of the buffers, in `(state, seq, sub)` order.
    pub fn drain(&self) -> Vec<Record> {
        let mut records: Vec<Record> = Vec::new();
        // Appended, so each buffer keeps its room for the records after the drain.
        for buffer in guard(&self.buffers).iter() {
            records.append(&mut guard(buffer).records);
        }
        records.sort_by_key(|record| (record.state, record.seq, record.sub));
        records
    }
}

/// The thread's entered collector ([`Collector::enter`]), left when dropped, on the thread that
/// entered it.
pub struct Entered {
    _thread: PhantomData<*const ()>,
}

impl Drop for Entered {
    fn drop(&mut self) {
        // A chaos worker leaves it as its thread-locals are torn down, maybe this one first.
        let _ = ENTERED.try_with(|slot| slot.borrow_mut().take());
    }
}

/// The task a checkpoint's reads, loads and save run as ([`recorded`]): none of a run's, and
/// small enough for the lock probe's holder slots, which keep a task's id plus one.
pub const CHECKPOINT_TASK: usize = 1 << 32;

/// A scheduler that records on its thread and parks nowhere: installed for what one task does
/// alone at a quiescent point, a checkpoint's reads, loads and save. Counts the futures that work
/// spawns while they live, so none outlives it unseen.
struct Recorder {
    log: Arc<Collector>,
    spawned: AtomicUsize,
}

impl hooks::Scheduler for Recorder {
    fn reached(&self, _point: Point) -> Option<Pin<Box<dyn Future<Output = ()> + Send>>> {
        self.log.reserve();
        None
    }

    fn touched(&self, _resource: Resource, _write: bool) {}

    fn spawned(&self, task: usize) -> usize {
        self.spawned.fetch_add(1, Ordering::Relaxed);
        task
    }

    fn ended(&self, _task: usize) {
        self.spawned.fetch_sub(1, Ordering::Relaxed);
    }

    /// The one thread blocks where a lock it needs is held: the deadlock fails the work, naming
    /// the holders, as a run's scheduler fails a run.
    fn contended(&self, contention: &hooks::Contention) {
        panic!(
            "a checkpoint's own work found {:?} held, taking it {:?}: holders {:?}",
            contention.block, contention.mode, contention.holders
        );
    }

    fn recording(&self) -> bool {
        true
    }

    fn effect(&self, record: Record) {
        self.log.record(record);
    }
}

/// `future`'s output and what it logged, run on this thread, which no scheduler holds, as
/// [`CHECKPOINT_TASK`] with recording on, its records in `(state, seq, sub)` order; a record made
/// with no task fails it, and so does a future it spawned still running once the runtime lets the
/// ones dropped with it end: one dropped that takes several turns to end fails it as well, which
/// no check or checkpoint drops. Its futures spawned on another thread record nothing.
pub async fn recorded<F: Future>(future: F) -> (F::Output, Vec<Record>) {
    assert_eq!(
        tokio::runtime::Handle::current().runtime_flavor(),
        tokio::runtime::RuntimeFlavor::CurrentThread,
        "the futures a checkpoint spawns run on its own thread"
    );
    let log = Collector::new();
    log.set_recording(true);
    log.reserve();
    let recorder = Arc::new(Recorder {
        log: log.clone(),
        spawned: AtomicUsize::new(0),
    });
    let installed = hooks::install(recorder.clone());
    let output = hooks::as_task(CHECKPOINT_TASK, future).await;
    // A `JoinSet` dropped on an early return drops its futures at the runtime's next turn.
    let mut live = recorder.spawned.load(Ordering::Relaxed);
    while live > 0 {
        tokio::task::yield_now().await;
        let now = recorder.spawned.load(Ordering::Relaxed);
        assert!(
            now < live,
            "a checkpoint's own work left {now} spawned futures running"
        );
        live = now;
    }
    drop(installed);
    if let Some(what) = log.no_task() {
        panic!("a checkpoint's own work: {what}");
    }
    (output, log.drain())
}

/// Tops `buffer` up where its room is short of the largest section, a discard's run over a whole
/// block, twice over.
fn top_up(buffer: &mut Vec<Record>) {
    if buffer.capacity() - buffer.len() < 4 * BLOCK_NODE_COUNT + 64 {
        buffer.reserve(8 * BLOCK_NODE_COUNT);
    }
}

/// A lock's guard, whatever panicked holding it: a panic in a task fails the run, which then
/// reads the records, so it must not fail it twice.
fn guard<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
