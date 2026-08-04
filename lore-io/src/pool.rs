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
const MAX_POOL_THREADS: usize = 128;

/// The pool cap override, read once when the process-wide pool is created.
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
pub(crate) fn default_max_threads() -> usize {
    let cores = std::thread::available_parallelism().map_or(2, |count| count.get());
    std::cmp::min(2 * cores, 16)
}

/// Parses a [`POOL_THREADS_VAR`] value. Separate from reading the variable so the accepted range
/// and the error are testable without a process-global environment.
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

/// The cap the process-wide pool is built with: [`default_max_threads`] unless
/// [`POOL_THREADS_VAR`] names a usable count.
///
/// The variable is for measurement and rollback — sweeping the cap against a real workload is how
/// the formula gets checked, and BENCHMARKS.md reports that no single value wins every phase — so
/// an unusable one reports itself and yields to the formula rather than failing a host
/// application's first file read. Sizing the engine for production stays with the `ThreadCounts`
/// apportionment in `lore-base`, which is what keeps a process inside the budget an embedder asked
/// for; this knob deliberately cannot exceed the pool it replaces.
fn configured_max_threads() -> usize {
    match std::env::var(POOL_THREADS_VAR) {
        Ok(value) => max_threads_from_value(&value).unwrap_or_else(|error| {
            eprintln!("lore-io: {error}; using the default pool size instead");
            default_max_threads()
        }),
        Err(_) => default_max_threads(),
    }
}

/// Submitted work and the slot its result is published into, in one allocation.
///
/// The queue holds it as a [`Job`] to run and the awaiting future holds it as a [`Completion`] to
/// read — two views of the same `Arc`. What this replaces is two heap allocations per operation, a
/// boxed closure and a channel, on the hottest path in the crate. It also leaves the crate with no
/// dependency on any async runtime, rather than one on a runtime-agnostic channel.
struct Task<T, F> {
    state: Mutex<TaskState<T, F>>,
}

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

struct PoolState {
    queue: VecDeque<Arc<dyn Job>>,
    idle: usize,
    running: usize,
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
}

struct PoolInner {
    state: Mutex<PoolState>,
    work_available: Condvar,
    max_threads: usize,
}

/// Bounded pool of threads dedicated to running blocking syscalls.
///
/// Threads are spawned on demand up to the configured maximum and exit
/// after a keep-alive period without work. Submitted work completes
/// through a runtime-independent future.
pub(crate) struct SyscallPool {
    inner: Arc<PoolInner>,
}

impl SyscallPool {
    pub(crate) fn new(max_threads: usize) -> SyscallPool {
        SyscallPool {
            inner: Arc::new(PoolInner {
                state: Mutex::new(PoolState {
                    queue: VecDeque::new(),
                    idle: 0,
                    running: 0,
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

    pub(crate) fn submit<T, F>(&self, work: F) -> SyscallTask<T>
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

        let spawn_worker = {
            let mut state = self.inner.state.lock();
            state.queue.push_back(job);
            state.queue_high_water = state.queue_high_water.max(state.queue.len());
            if state.idle > 0 {
                self.inner.work_available.notify_one();
                false
            } else if state.running < self.inner.max_threads {
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

        SyscallTask { completion }
    }

    /// A snapshot of the pool's occupancy. Taken under the pool's own lock, so it is consistent
    /// rather than assembled from independently sampled counters.
    pub(crate) fn stats(&self) -> PoolStats {
        let state = self.inner.state.lock();
        PoolStats {
            queued: state.queue.len(),
            executing: state.running.saturating_sub(state.idle),
            threads: state.running,
            threads_high_water: state.threads_high_water,
            queue_high_water: state.queue_high_water,
            max_threads: self.inner.max_threads,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A queue entry for the tests that put work in by hand rather than through `submit`, whose
    /// result nobody reads.
    fn queued_job(work: impl FnOnce() + Send + 'static) -> Arc<dyn Job> {
        Arc::new(Task {
            state: Mutex::new(TaskState::Pending {
                work: Some(work),
                waker: None,
            }),
        })
    }

    #[tokio::test]
    async fn submit_runs_work_and_returns_result() {
        let pool = SyscallPool::new(2);
        let value = pool.submit(|| 7 * 6).await;
        assert_eq!(value, 42);
    }

    #[tokio::test]
    async fn submissions_beyond_thread_cap_complete() {
        let pool = SyscallPool::new(2);
        let tasks: Vec<_> = (0..64).map(|i| pool.submit(move || i * 2)).collect();
        for (i, task) in tasks.into_iter().enumerate() {
            assert_eq!(task.await, i * 2);
        }
    }

    #[tokio::test]
    #[should_panic(expected = "deliberate")]
    async fn panic_on_pool_thread_resumes_on_awaiter() {
        let pool = SyscallPool::new(1);
        pool.submit(|| panic!("deliberate")).await;
    }

    /// The snapshot is taken under one lock, so a burst cannot be observed with more threads
    /// than the cap allows or with counts that do not add up.
    #[tokio::test(flavor = "multi_thread")]
    async fn stats_stay_within_the_thread_cap_through_a_burst() {
        let pool = SyscallPool::new(4);
        let tasks: Vec<_> = (0..64).map(|index| pool.submit(move || index)).collect();
        for (index, task) in tasks.into_iter().enumerate() {
            assert_eq!(task.await, index);
        }

        let stats = pool.stats();
        assert_eq!(stats.max_threads, 4);
        assert!(stats.threads <= 4, "{} threads alive", stats.threads);
        assert!(
            stats.threads_high_water <= 4,
            "{} threads at peak",
            stats.threads_high_water
        );
        assert!(stats.threads_high_water >= 1, "no thread was ever spawned");
        assert!(stats.queue_high_water >= 1, "the queue was never occupied");
        assert_eq!(stats.queued, 0, "a job was left queued");
    }

    /// With no worker alive, a queued job has nobody to run it, so the submitting thread must
    /// run it and must hand back the slot it reserved for the thread the OS refused. Leaving
    /// either undone is a task that never completes.
    #[test]
    fn a_refused_worker_thread_releases_its_slot_and_runs_the_job() {
        let pool = SyscallPool::new(2);
        let ran = Arc::new(AtomicUsize::new(0));
        let flag = Arc::clone(&ran);
        {
            let mut state = pool.inner.state.lock();
            state.queue.push_back(queued_job(move || {
                flag.fetch_add(1, Ordering::SeqCst);
            }));
            state.running += 1;
        }

        pool.run_without_worker();

        assert_eq!(ran.load(Ordering::SeqCst), 1, "the job never ran");
        let state = pool.inner.state.lock();
        assert_eq!(state.running, 0, "the reserved slot was not released");
        assert!(state.queue.is_empty());
    }

    /// A live worker will drain the queue, so blocking the caller would buy nothing.
    #[test]
    fn a_refused_worker_thread_leaves_the_job_when_one_is_alive() {
        let pool = SyscallPool::new(3);
        let ran = Arc::new(AtomicUsize::new(0));
        let flag = Arc::clone(&ran);
        {
            let mut state = pool.inner.state.lock();
            state.queue.push_back(queued_job(move || {
                flag.fetch_add(1, Ordering::SeqCst);
            }));
            state.running += 2;
        }

        pool.run_without_worker();

        assert_eq!(
            ran.load(Ordering::SeqCst),
            0,
            "the live worker's job was taken"
        );
        let state = pool.inner.state.lock();
        assert_eq!(state.running, 1, "only the refused slot comes back");
        assert_eq!(state.queue.len(), 1);
    }

    /// The default is a tuned position on a measured curve, not an arbitrary number, so it is
    /// pinned: below the workload's concurrency the cold whole-file phases degrade sharply, and
    /// above this ceiling the throughput measured did not pay for the threads.
    #[test]
    fn the_default_pool_size_stays_within_its_measured_range() {
        let default = default_max_threads();
        assert!(default >= 1, "a pool of {default} threads runs nothing");
        assert!(
            default <= 16,
            "{default} threads exceeds the measured ceiling"
        );
        assert!(
            default <= MAX_POOL_THREADS,
            "the default must be reachable through the override"
        );
    }

    #[test]
    fn a_pool_size_override_is_taken_as_written() {
        assert_eq!(max_threads_from_value("16").expect("16 is usable"), 16);
        assert_eq!(
            max_threads_from_value(" 8 \n").expect("surrounding space is not a typo worth failing"),
            8
        );
        assert_eq!(
            max_threads_from_value(&MAX_POOL_THREADS.to_string()).expect("the ceiling is usable"),
            MAX_POOL_THREADS
        );
    }

    /// Every rejected value names itself and the range, because the reader of the message is
    /// someone who has just set the variable and needs to know what to set it to instead.
    #[test]
    fn an_unusable_pool_size_override_is_rejected_with_its_range() {
        for value in ["0", "-4", "many", "", &(MAX_POOL_THREADS + 1).to_string()] {
            let error = max_threads_from_value(value)
                .expect_err("an unusable override must not reach the pool");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert!(
                error.to_string().contains(POOL_THREADS_VAR),
                "\"{value}\" was rejected without naming the variable: {error}"
            );
        }
    }

    /// A signal arriving before the call makes progress must not reach the caller as a
    /// failure: the operation is retried and its real outcome is what surfaces.
    #[test]
    fn an_interrupted_call_is_retried_until_it_makes_progress() {
        let mut attempts = 0;
        let read = retry_on_interrupt(|| {
            attempts += 1;
            if attempts < 3 {
                Err(std::io::Error::from(std::io::ErrorKind::Interrupted))
            } else {
                Ok(64)
            }
        })
        .expect("the successful attempt must surface");

        assert_eq!(read, 64);
        assert_eq!(attempts, 3);
    }

    use std::sync::atomic::AtomicBool;

    /// A value that reports its own destruction.
    struct DropCounted(Arc<AtomicUsize>);

    impl Drop for DropCounted {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Submits work returning a [`DropCounted`], abandons the future, and waits for the value to
    /// be destroyed. Panics if it never is.
    ///
    /// The work waits for a gate the caller opens only after dropping the future, so the result is
    /// always published to an awaiter that is already gone. Letting the job race the drop instead
    /// would sometimes complete it first, which is a different state and not the one under test.
    fn assert_an_abandoned_result_is_dropped(poll_before_dropping: bool) {
        let pool = SyscallPool::new(1);
        let drops = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(AtomicBool::new(false));

        let payload = Arc::clone(&drops);
        let gate = Arc::clone(&release);
        let mut task = Box::pin(pool.submit(move || {
            while !gate.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            DropCounted(payload)
        }));
        if poll_before_dropping {
            let mut context = Context::from_waker(Waker::noop());
            assert!(task.as_mut().poll(&mut context).is_pending());
        }
        drop(task);
        release.store(true, Ordering::SeqCst);

        for _ in 0..2000 {
            if drops.load(Ordering::SeqCst) == 1 {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("an abandoned result was never dropped");
    }

    /// A result nobody takes is still destroyed. Dropping the future abandons the result rather
    /// than the work, so the value the job publishes has no reader — and for a read that value is
    /// the whole buffer. Leaving it parked in the task allocation leaks memory in proportion to
    /// cancelled reads, silently: no operation fails and no assertion elsewhere notices.
    #[test]
    fn an_abandoned_result_is_dropped_when_the_future_was_never_polled() {
        assert_an_abandoned_result_is_dropped(false);
    }

    /// The same, for a future that registered a waker before going away. The job then has a waker
    /// to wake and a result to publish with the awaiter already gone, which is a different path
    /// through the state than never having been polled at all.
    #[test]
    fn an_abandoned_result_is_dropped_after_a_poll_registered_a_waker() {
        assert_an_abandoned_result_is_dropped(true);
    }

    /// Every other error is the caller's to see, on the first attempt.
    #[test]
    fn other_errors_are_not_retried() {
        let mut attempts = 0;
        let error = retry_on_interrupt(|| -> std::io::Result<usize> {
            attempts += 1;
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        })
        .expect_err("a non-interrupt error must not be retried");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(attempts, 1);
    }
}
