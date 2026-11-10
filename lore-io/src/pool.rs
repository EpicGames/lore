// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::VecDeque;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;

use parking_lot::Condvar;
use parking_lot::Mutex;

const KEEP_ALIVE: Duration = Duration::from_secs(10);

/// Largest pool the override will hand out. A thread is only spawned when there is queued work
/// and no idle thread, so an absurd value costs nothing until a burst arrives — and then costs
/// that many threads at once. The ceiling is the tokio blocking pool's, so the knob cannot size
/// the engine above the pool it replaces.
#[lore_macro::test_pub]
const MAX_POOL_THREADS: usize = 128;

/// The pool cap override, read once when the process-wide pool is created.
#[lore_macro::test_pub]
const POOL_THREADS_VAR: &str = "LORE_IO_POOL_THREADS";

/// Default syscall pool size: `min(2 × cores, 16)`.
///
/// Sized to absorb syscalls that block on slow media (cold cache, network filesystems) without
/// stalling async worker threads, while keeping this pool's claim on the process-wide thread budget
/// small enough to leave room for the populations it shares that budget with.
///
/// Doubling the cap buys 4–6% warm and 2–8% cold on Windows/NTFS, and nothing outside ±4% on
/// Linux/ext4. No cap wins every phase, so this is a position on a curve rather than an optimum;
/// `lore-io/BENCHMARKS.md` has the sweeps. What does cost is falling well below the workload's
/// concurrency: 8 threads measured 0.54× against 32 on 16,384 evicted files read at 64-way
/// concurrency, and 4 threads measured 0.65× on macOS/APFS cold reads offering 64 and 128. That
/// ratio is pool size against in-flight requests rather than against core count, so it is what a
/// machine small enough for `2 × cores` to reach those sizes runs into.
#[lore_macro::test_pub]
pub(crate) fn default_max_threads() -> usize {
    let cores = std::thread::available_parallelism().map_or(2, |count| count.get());
    std::cmp::min(2 * cores, 16)
}

/// Parses a [`POOL_THREADS_VAR`] value. Separate from reading the variable so the accepted range
/// and the error are testable without a process-global environment.
#[lore_macro::test_pub]
fn max_threads_from_value(value: &str) -> std::io::Result<usize> {
    let invalid = |detail: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, detail);
    let count: usize = value.trim().parse().map_err(|error| {
        invalid(format!(
            "unsupported {POOL_THREADS_VAR} \"{value}\": {error} \
             (expected a whole number of threads)"
        ))
    })?;
    if count == 0 {
        return Err(invalid(format!(
            "{POOL_THREADS_VAR} must be at least 1; a pool of 0 threads runs nothing"
        )));
    }
    if count > MAX_POOL_THREADS {
        return Err(invalid(format!(
            "{POOL_THREADS_VAR} of {count} exceeds the {MAX_POOL_THREADS}-thread ceiling"
        )));
    }
    Ok(count)
}

/// The pool size this crate asks for on its own: [`default_max_threads`] unless
/// [`POOL_THREADS_VAR`] names a usable count.
///
/// The variable is for measurement and rollback — sweeping the cap against a real workload is how
/// the formula gets checked, and BENCHMARKS.md reports that no single value wins every phase — so
/// an unusable one reports itself and yields to the formula rather than failing a host
/// application's first file read. It is a request rather than a final size: sizing the engine for
/// production stays with the `ThreadCounts` apportionment in `lore-base`, which reads this and
/// hands back the pool's share of the budget through [`set_max_threads`].
pub fn requested_max_threads() -> usize {
    match std::env::var(POOL_THREADS_VAR) {
        Ok(value) => max_threads_from_value(&value).unwrap_or_else(|error| {
            eprintln!("lore-io: {error}; using the default pool size instead");
            default_max_threads()
        }),
        Err(_) => default_max_threads(),
    }
}

/// The pool's share of the process thread budget, set before the pool is built.
static BUDGETED_MAX_THREADS: OnceLock<usize> = OnceLock::new();

/// Sets the pool's cap from the process thread budget, taking precedence over
/// [`requested_max_threads`].
///
/// `lore-base` calls this as it sizes the runtime, so a total thread limit binds this pool as it
/// binds the others rather than leaving the engine to grow outside it. Must run before the first
/// file operation, which is what builds the pool; returns false if a cap was already set.
pub fn set_max_threads(count: usize) -> bool {
    BUDGETED_MAX_THREADS.set(count.max(1)).is_ok()
}

/// The cap the process-wide pool is built with: its budgeted share when one has been set, else
/// what the crate asks for on its own.
fn configured_max_threads() -> usize {
    BUDGETED_MAX_THREADS
        .get()
        .copied()
        .unwrap_or_else(requested_max_threads)
}

/// Submitted work and the slot its result is published into, in one allocation.
///
/// The queue holds it as a [`Job`] to run and the awaiting future holds it as a [`Completion`] to
/// read — two views of the same `Arc`. What this replaces is two heap allocations per operation, a
/// boxed closure and a channel, on the hottest path in the crate. It also leaves the crate with no
/// dependency on any async runtime, rather than one on a runtime-agnostic channel.
#[lore_macro::test_pub]
struct Task<T, F> {
    state: Mutex<TaskState<T, F>>,
}

#[lore_macro::test_pub]
enum TaskState<T, F> {
    /// Not finished. `work` is taken when a thread starts it; `waker` is left by a poll that
    /// arrived first, which is why both can be present at once.
    Pending {
        work: Option<F>,
        waker: Option<Waker>,
    },
    /// Finished, result not yet taken. `Err` carries a panic to resume on the awaiter.
    Ready(std::thread::Result<T>),
    /// Result taken.
    Taken,
}

/// The runnable view of a [`Task`], which is what the queue holds.
#[lore_macro::test_pub]
trait Job: Send + Sync {
    /// Runs the work and publishes its result, waking the awaiter if one is waiting. Does nothing
    /// if the work has already been taken, so a second call cannot run it twice.
    fn run(&self);
}

/// The awaitable view of a [`Task`], which is what [`SyscallTask`] holds.
trait Completion<T>: Send + Sync {
    fn poll_result(&self, context: &mut Context<'_>) -> Poll<std::thread::Result<T>>;
}

impl<T, F> Job for Task<T, F>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    fn run(&self) {
        let work = match &mut *self.state.lock() {
            TaskState::Pending { work, .. } => work.take(),
            _ => None,
        };
        let Some(work) = work else {
            return;
        };

        // Deliberately not under the lock: the work is a blocking syscall, and the awaiter polls
        // while it runs.
        let result = std::panic::catch_unwind(AssertUnwindSafe(work));

        let waker = {
            let mut state = self.state.lock();
            let waker = match &mut *state {
                TaskState::Pending { waker, .. } => waker.take(),
                _ => None,
            };
            *state = TaskState::Ready(result);
            waker
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T, F> Completion<T> for Task<T, F>
where
    T: Send + 'static,
    F: Send + 'static,
{
    fn poll_result(&self, context: &mut Context<'_>) -> Poll<std::thread::Result<T>> {
        let mut state = self.state.lock();
        match &mut *state {
            TaskState::Pending { waker, .. } => {
                *waker = Some(context.waker().clone());
                Poll::Pending
            }
            TaskState::Ready(_) => match std::mem::replace(&mut *state, TaskState::Taken) {
                TaskState::Ready(result) => Poll::Ready(result),
                _ => unreachable!("the state was Ready under this lock"),
            },
            TaskState::Taken => panic!("a syscall task was polled after it completed"),
        }
    }
}

/// Runs `call`, retrying for as long as the syscall reports it was interrupted by a signal.
///
/// `EINTR` says a signal handler ran before the call made progress. It carries no cancellation
/// intent — the process need not even be shutting down, and a handler installed with
/// `SA_RESTART`, which is what the signal handling above this crate uses, has the kernel restart
/// an interrupted call so that nothing surfaces here at all. Retrying is therefore what `std`'s
/// own wrappers do, and what the file paths this crate replaces did; propagating instead would
/// report a failure for an operation that had not failed. Only operations that document `EINTR`
/// and are idempotent under retry are wrapped.
///
/// Deliberate cancellation is a separate mechanism and never arrives here: dropping the future
/// abandons the result while the submitted job runs to completion, and the completion backends
/// report an aborted operation as its own status, interpreted where it arrives.
#[lore_macro::test_pub]
pub(crate) fn retry_on_interrupt<T>(
    mut call: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    loop {
        match call() {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

#[lore_macro::test_pub]
struct PoolState {
    queue: VecDeque<Arc<dyn Job>>,
    idle: usize,
    running: usize,
    /// Threads of the pool's cap lent out by [`SyscallPool::lend`], which the pool starts no
    /// thread of its own for while they are lent.
    lent: usize,
    queue_high_water: usize,
    threads_high_water: usize,
}

/// A snapshot of the syscall pool's occupancy.
///
/// The high-water marks are what the thread budget needs in order to be checked against reality:
/// a cap is only the right size if the workload actually reaches it, and a queue that runs deep
/// while threads sit idle means the cap is not the limit.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PoolStats {
    /// Jobs submitted and not yet started.
    pub queued: usize,
    /// Jobs currently running on a pool thread.
    pub executing: usize,
    /// Threads alive, idle ones included.
    pub threads: usize,
    /// Most threads alive at once since the pool was created.
    pub threads_high_water: usize,
    /// Deepest the queue has been since the pool was created.
    pub queue_high_water: usize,
    /// The cap `threads` never exceeds.
    pub max_threads: usize,
    /// Threads of the cap lent out, which `threads` stays below the cap by once the idle threads
    /// past it have woken and ended.
    pub lent: usize,
}

#[lore_macro::test_pub]
struct PoolInner {
    state: Mutex<PoolState>,
    work_available: Condvar,
    max_threads: usize,
}

/// Threads of a [`SyscallPool`]'s cap that [`SyscallPool::lend`] lent, given back when dropped.
#[lore_macro::test_pub]
pub(crate) struct Lent<'a> {
    pool: &'a SyscallPool,
    count: usize,
}

impl Lent<'_> {
    /// How many threads were lent.
    #[lore_macro::test_pub]
    pub(crate) fn count(&self) -> usize {
        self.count
    }

    /// Takes over the threads `other` holds, which the same pool lent, so they are given back with
    /// these.
    #[lore_macro::test_pub]
    pub(crate) fn absorb(&mut self, mut other: Lent<'_>) {
        debug_assert!(std::ptr::eq(self.pool, other.pool), "lent by another pool");
        self.count += std::mem::take(&mut other.count);
    }

    /// Gives `count` of the threads back before the rest, as when the work they were lent to
    /// could not start them, and starts a thread in each slot given back for a job that queued
    /// while they were lent and that no idle thread will take, so the queue drains at the full
    /// cap again rather than waiting for a later submission to start one.
    #[lore_macro::test_pub]
    pub(crate) fn give_back(&mut self, count: usize) {
        let count = count.min(self.count);
        if count == 0 {
            return;
        }
        self.count -= count;
        let inner = &self.pool.inner;
        let spawn = {
            let mut state = inner.state.lock();
            state.lent -= count;
            let room = inner.max_threads.saturating_sub(state.running + state.lent);
            let spawn = room.min(state.queue.len().saturating_sub(state.idle));
            state.running += spawn;
            state.threads_high_water = state.threads_high_water.max(state.running);
            spawn
        };
        for started in 0..spawn {
            if spawn_worker_thread(Arc::clone(inner)).is_err() {
                self.pool.run_without_workers(spawn - started);
                break;
            }
        }
    }
}

impl Drop for Lent<'_> {
    /// Gives every thread still lent back, as [`Lent::give_back`] does.
    fn drop(&mut self) {
        self.give_back(self.count);
    }
}

/// Bounded pool of threads dedicated to running blocking syscalls.
///
/// Threads are spawned on demand up to the configured maximum and exit
/// after a keep-alive period without work. Submitted work completes
/// through a runtime-independent future.
#[lore_macro::test_pub]
pub(crate) struct SyscallPool {
    inner: Arc<PoolInner>,
}

impl SyscallPool {
    #[lore_macro::test_pub]
    pub(crate) fn new(max_threads: usize) -> SyscallPool {
        SyscallPool {
            inner: Arc::new(PoolInner {
                state: Mutex::new(PoolState {
                    queue: VecDeque::new(),
                    idle: 0,
                    running: 0,
                    lent: 0,
                    queue_high_water: 0,
                    threads_high_water: 0,
                }),
                work_available: Condvar::new(),
                max_threads,
            }),
        }
    }

    pub(crate) fn global() -> &'static SyscallPool {
        static GLOBAL: OnceLock<SyscallPool> = OnceLock::new();
        GLOBAL.get_or_init(|| SyscallPool::new(configured_max_threads()))
    }

    /// Lends up to `wanted` threads of the pool's cap to work that runs them itself, for as long
    /// as the [`Lent`] is held. The pool lends at most half its cap in all, and no thread busy with
    /// work; its idle threads past the cap end as they wake. So, once those have ended, the threads
    /// it runs and the threads lent stay within its cap, and at least half the cap stays for every
    /// other file operation. May lend none.
    #[lore_macro::test_pub]
    pub(crate) fn lend(&self, wanted: usize) -> Lent<'_> {
        let mut state = self.inner.state.lock();
        let busy = state.running - state.idle;
        let lendable = (self.inner.max_threads / 2)
            .saturating_sub(state.lent)
            .min(self.inner.max_threads.saturating_sub(busy + state.lent));
        let count = wanted.min(lendable);
        state.lent += count;
        if count > 0 {
            // Lent threads can leave more threads running than the cap allows; idle ones end at
            // once rather than at their keep-alive.
            self.inner.work_available.notify_all();
        }
        Lent { pool: self, count }
    }

    #[lore_macro::test_pub]
    pub(crate) fn submit<T, F>(&self, work: F) -> SyscallTask<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (job, task) = new_task(work);
        self.enqueue(job);
        task
    }

    /// Runs `work` on a thread of its own, outside the pool and its cap, for work that holds its
    /// thread for minutes and would hold a pool thread from every other file operation as long.
    /// The thread is named `name` and ends with the work. It counts against no pool cap: each call
    /// adds a thread for as long as its work runs. Where the system refuses the thread, the work
    /// goes to the pool.
    #[lore_macro::test_pub]
    pub(crate) fn run_on_own_thread<T, F>(&self, name: &str, work: F) -> SyscallTask<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (job, task) = new_task(work);
        let runner = Arc::clone(&job);
        if std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || runner.run())
            .is_err()
        {
            self.enqueue(job);
        }
        task
    }

    /// Queues `job` for a pool thread, starting one where none is idle and the cap allows.
    fn enqueue(&self, job: Arc<dyn Job>) {
        let spawn_worker = {
            let mut state = self.inner.state.lock();
            state.queue.push_back(job);
            state.queue_high_water = state.queue_high_water.max(state.queue.len());
            if state.idle > 0 {
                self.inner.work_available.notify_one();
                false
            } else if state.running + state.lent < self.inner.max_threads {
                state.running += 1;
                state.threads_high_water = state.threads_high_water.max(state.running);
                true
            } else {
                false
            }
        };
        if spawn_worker && spawn_worker_thread(Arc::clone(&self.inner)).is_err() {
            self.run_without_worker();
        }
    }

    /// A snapshot of the pool's occupancy. Taken under the pool's own lock, so it is consistent
    /// rather than assembled from independently sampled counters.
    #[lore_macro::test_pub]
    pub(crate) fn stats(&self) -> PoolStats {
        let state = self.inner.state.lock();
        PoolStats {
            queued: state.queue.len(),
            executing: state.running.saturating_sub(state.idle),
            threads: state.running,
            threads_high_water: state.threads_high_water,
            queue_high_water: state.queue_high_water,
            max_threads: self.inner.max_threads,
            lent: state.lent,
        }
    }

    /// Recovers from the operating system refusing the first of `unstarted` worker threads whose
    /// slots were reserved together: gives back the slots of the threads after it, then recovers
    /// from the refused one as [`Self::run_without_worker`] does.
    #[lore_macro::test_pub]
    fn run_without_workers(&self, unstarted: usize) {
        // The slots of the threads after the refused one are given back first, as its own is.
        self.inner.state.lock().running -= unstarted - 1;
        self.run_without_worker();
    }

    /// Recovers from the operating system refusing a worker thread.
    ///
    /// The slot reserved for the refused thread is given back first, so a later submission
    /// tries the spawn again rather than believing the pool is at capacity — leaving it
    /// reserved is what would let repeated refusals saturate the pool with threads that do not
    /// exist, after which nothing runs and every task waits forever.
    ///
    /// With no worker left alive, a queued job has nobody to run it, so this thread runs one.
    /// Blocking the caller is the point: the alternative is a task that never completes. The
    /// oldest job goes first, keeping the pool's order, and the caller's own job is drained by
    /// whichever submission comes next — the same submission that retries the spawn. While any
    /// worker is still alive the queue is left to it.
    #[lore_macro::test_pub]
    fn run_without_worker(&self) {
        let job = {
            let mut state = self.inner.state.lock();
            state.running -= 1;
            if state.running == 0 {
                state.queue.pop_front()
            } else {
                None
            }
        };
        if let Some(job) = job {
            job.run();
        }
    }
}

/// A task for `work`, as the queue or a thread runs it and as its caller awaits it.
fn new_task<T, F>(work: F) -> (Arc<dyn Job>, SyscallTask<T>)
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let task = Arc::new(Task {
        state: Mutex::new(TaskState::Pending {
            work: Some(work),
            waker: None,
        }),
    });
    let job: Arc<dyn Job> = task.clone();
    let completion: Arc<dyn Completion<T>> = task;
    (job, SyscallTask { completion })
}

/// Starts a worker thread, reporting refusal rather than panicking: a host that has already
/// exhausted its thread budget is exactly the environment this pool is sized for, and the
/// caller has a reserved slot to give back.
fn spawn_worker_thread(inner: Arc<PoolInner>) -> std::io::Result<()> {
    static THREAD_ID: AtomicUsize = AtomicUsize::new(0);
    let id = THREAD_ID.fetch_add(1, Ordering::Relaxed);
    std::thread::Builder::new()
        .name(format!("lore-io-{id}"))
        .spawn(move || worker_loop(&inner))
        .map(|_| ())
}

fn worker_loop(inner: &PoolInner) {
    loop {
        let job = {
            let mut state = inner.state.lock();
            loop {
                // Lending can leave more threads running than the cap allows; the surplus ends
                // here, and the threads within the cap take the queued work.
                if state.running + state.lent > inner.max_threads {
                    state.running -= 1;
                    break None;
                }
                if let Some(job) = state.queue.pop_front() {
                    break Some(job);
                }
                state.idle += 1;
                let timed_out = inner
                    .work_available
                    .wait_for(&mut state, KEEP_ALIVE)
                    .timed_out();
                state.idle -= 1;
                if timed_out && state.queue.is_empty() {
                    state.running -= 1;
                    break None;
                }
            }
        };
        match job {
            Some(job) => job.run(),
            None => return,
        }
    }
}

/// Completion future for work submitted to the syscall pool.
///
/// Wakes through a plain waker and therefore runs under any executor. A
/// panic on the pool thread resumes on the awaiting task.
///
/// Dropping this cancels nothing: the work owns its buffer and runs to completion, publishing a
/// result nobody reads, and the allocation — with that buffer in it — is freed when the pool
/// thread releases its own handle. That is what makes an early free of kernel-visible memory
/// unrepresentable rather than merely avoided.
///
/// Work that is discarded rather than run leaves this pending forever. Only a pool dropped with a
/// non-empty queue can do that, which the process-wide pool never is. Detecting it here is not
/// worth attempting: whether a result can still arrive is only knowable under the state lock, and
/// a check outside it reads a completed job as a discarded one.
#[lore_macro::test_pub]
pub(crate) struct SyscallTask<T> {
    completion: Arc<dyn Completion<T>>,
}

impl<T> Future for SyscallTask<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        match self.completion.poll_result(context) {
            Poll::Ready(Ok(value)) => Poll::Ready(value),
            Poll::Ready(Err(panic)) => std::panic::resume_unwind(panic),
            Poll::Pending => Poll::Pending,
        }
    }
}
