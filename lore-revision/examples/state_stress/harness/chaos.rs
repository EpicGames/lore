// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Chaos on threads: a scheduler that yields a task at random scheduling points and stalls its
//! thread at random in the windows no await separates, installed on every worker of a
//! multi-thread runtime, so the windows between a node operation's steps open wide enough for
//! other threads to run in them.

use std::cell::Cell;
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use lore_revision::state::node::hooks;
use lore_revision::state::node::hooks::Point;
use lore_revision::state::node::hooks::Resource;
use lore_revision::state::node::hooks::Window;
use lore_revision::state::node::hooks::log::Record;

use crate::harness::catalogue::call::InstalledSpawns;
use crate::harness::catalogue::call::Spawn;
use crate::harness::catalogue::call::Spawns;
use crate::harness::log::Collector;
use crate::harness::log::Entered;
use crate::harness::mixes::xorshift;

/// Yields at one scheduling point in `every`, on average, the walks' own points included, and at
/// one level of a served mark's descent in two, and stalls its thread in one window in `every`,
/// choosing from a per-thread seed.
pub struct Chaos {
    every: u64,
    /// The tasks' yields and the threads' stalls in windows.
    yields: Padded<AtomicU64>,
    /// Locks the probe found held.
    contentions: Padded<AtomicU64>,
    /// The log, off until a driver turns it on.
    log: Arc<Collector>,
    /// How each task came to be, once a driver numbered its operations' tasks
    /// ([`Self::number_tasks`]); till then a spawned future runs as its spawner's task.
    spawns: Arc<Spawns>,
    /// Whether tasks yield only at points a future can be dropped at ([`Point::droppable`]), so a
    /// driver that drops tasks drops each only where a caller could ([`Self::yield_droppable`]).
    droppable: AtomicBool,
}

impl Chaos {
    /// How many times a task yielded or a thread stalled in a window.
    pub fn yields(&self) -> u64 {
        self.yields.0.load(Ordering::Relaxed)
    }

    /// How many times a probe found a block's lock held, told before the taker blocked.
    pub fn contentions(&self) -> u64 {
        self.contentions.0.load(Ordering::Relaxed)
    }

    /// The log the workers record into while it records.
    pub fn log(&self) -> &Arc<Collector> {
        &self.log
    }

    /// Numbers the first `tasks` tasks as the driver's operations' own: from here each future
    /// spawned through `hooks::inherit` runs as a task of its own, numbered after them, and
    /// [`Self::spawns`] tells how each came to be.
    pub fn number_tasks(&self, tasks: usize) {
        self.spawns.reserve(tasks);
    }

    /// How each task came to be, by task, once [`Self::number_tasks`] numbered them.
    pub fn spawns(&self) -> Vec<Spawn> {
        self.spawns.spawns()
    }

    /// Yields from here only at points a future can be dropped at, for a driver that aborts tasks:
    /// an abort drops a task where it is suspended, and a yield where no await stands outside the
    /// scheduler would drop it where no caller can.
    pub fn yield_droppable(&self) {
        self.droppable.store(true, Ordering::Relaxed);
    }
}

/// A counter on a cache line of its own, so the counts the workers write do not share one with the
/// fields every section reads.
#[repr(align(128))]
struct Padded<T>(T);

thread_local! {
    static STATE: Cell<u64> = const { Cell::new(0) };
    static INSTALLED: RefCell<Option<(hooks::Installed, Entered, InstalledSpawns)>> =
        const { RefCell::new(None) };
}

/// Whether the thread stops here, at one draw in `every` on average from its seed.
fn drawn(every: u64) -> bool {
    STATE.with(|state| {
        let mut value = state.get();
        let draw = xorshift(&mut value);
        state.set(value);
        draw.is_multiple_of(every)
    })
}

impl hooks::Scheduler for Chaos {
    fn reached(&self, point: Point) -> Option<Pin<Box<dyn Future<Output = ()> + Send>>> {
        // Outside every section here: the log's buffer grows here, not inside one.
        if self.log.recording() {
            self.log.reserve();
        }
        if !point.droppable() && self.droppable.load(Ordering::Relaxed) {
            return None;
        }
        let every = if point == Point::DescentStep {
            2
        } else {
            self.every
        };
        if !drawn(every) {
            return None;
        }
        self.yields.0.fetch_add(1, Ordering::Relaxed);
        Some(Box::pin(tokio::task::yield_now()))
    }

    /// A stall gives the thread's core up with no await, so no future can be dropped here, whether
    /// or not the driver drops tasks.
    fn window(&self, _window: Window) {
        if drawn(self.every) {
            self.yields.0.fetch_add(1, Ordering::Relaxed);
            std::thread::yield_now();
        }
    }

    fn touched(&self, _resource: Resource, _write: bool) {}

    fn spawned(&self, task: usize) -> usize {
        if self.spawns.numbered() {
            self.spawns.spawned(task)
        } else {
            task
        }
    }

    fn reader_points(&self) -> bool {
        true
    }

    fn contended(&self, _contention: &hooks::Contention) {
        self.contentions.0.fetch_add(1, Ordering::Relaxed);
    }

    fn recording(&self) -> bool {
        self.log.recording()
    }

    fn effect(&self, record: Record) {
        self.log.record(record);
    }
}

/// A multi-thread runtime of `workers` threads, each yielding at one scheduling point in
/// `every`, seeded from `seed` and the thread's start order, and the scheduler they share.
pub fn runtime(workers: usize, every: u64, seed: u64) -> (tokio::runtime::Runtime, Arc<Chaos>) {
    let chaos = Arc::new(Chaos {
        every,
        yields: Padded(AtomicU64::new(0)),
        contentions: Padded(AtomicU64::new(0)),
        log: Collector::new(),
        spawns: Arc::new(Spawns::default()),
        droppable: AtomicBool::new(false),
    });
    let installs = chaos.clone();
    let started = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .on_thread_start(move || {
            let order = started.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            STATE.with(|state| {
                state.set((seed ^ (order + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15)) | 1);
            });
            installs.log.register();
            let installed = hooks::install(installs.clone());
            let entered = installs.log.enter();
            let spawns = installs.spawns.install();
            INSTALLED.with_borrow_mut(|slot| *slot = Some((installed, entered, spawns)));
        })
        .on_thread_stop(|| {
            INSTALLED.with_borrow_mut(|slot| slot.take());
        })
        .build()
        .expect("the runtime starts");
    (runtime, chaos)
}
