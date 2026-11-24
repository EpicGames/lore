// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Waker;
use std::time::Duration;
use std::time::Instant;

use lore_io::pool::*;
use parking_lot::Mutex;

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

/// Work run on its own thread runs on a thread of that name and takes no pool thread, so the
/// pool, here of one thread, serves other work while it runs.
#[tokio::test]
async fn work_on_its_own_thread_leaves_the_pool_free() {
    let pool = SyscallPool::new(1);
    let (release, released) = std::sync::mpsc::channel::<()>();

    let own = pool.run_on_own_thread("lore-io-test-own-thread", move || {
        released.recv().expect("the release");
        std::thread::current().name().map(str::to_string)
    });
    assert_eq!(pool.submit(|| 7 * 6).await, 42);
    release.send(()).expect("release the work");

    assert_eq!(own.await.as_deref(), Some("lore-io-test-own-thread"));
    assert_eq!(
        pool.stats().threads_high_water,
        1,
        "only the pool's own work"
    );
}

/// Work submitted inside `run_file_operations_on_this_thread` has run, on the submitting thread,
/// by the time `submit` returns, and no pool thread is started for it.
#[test]
fn work_submitted_inside_the_call_runs_on_the_submitting_thread() {
    let pool = SyscallPool::new(2);
    let caller = std::thread::current().id();
    let ran = Arc::new(AtomicUsize::new(0));

    let task = run_file_operations_on_this_thread(|| {
        let counted = ran.clone();
        let task = pool.submit(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            std::thread::current().id()
        });
        assert_eq!(ran.load(Ordering::SeqCst), 1, "run at the submission");
        task
    });

    let mut task = std::pin::pin!(task);
    let std::task::Poll::Ready(ran_on) =
        task.as_mut().poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("the result is ready without a pool thread");
    };
    assert_eq!(ran_on, caller);
    assert_eq!(pool.stats().threads_high_water, 0, "no pool thread");
}

/// The thread submits to the pool again once `run_file_operations_on_this_thread` returns, a
/// nested call leaves the outer one in force, and another thread's submissions go to the pool
/// meanwhile.
#[tokio::test]
async fn the_thread_goes_back_to_the_pool_after_the_call() {
    let pool = Arc::new(SyscallPool::new(1));
    let caller = std::thread::current().id();
    let submitted_by = |pool: &SyscallPool| pool.submit(|| std::thread::current().id());

    let (after_nested, elsewhere) = run_file_operations_on_this_thread(|| {
        run_file_operations_on_this_thread(|| {});
        let after_nested = submitted_by(&pool);

        let other_pool = pool.clone();
        let elsewhere = std::thread::spawn(move || {
            let submitter = std::thread::current().id();
            (submitter, submitted_by(&other_pool))
        });
        (after_nested, elsewhere.join().expect("the other thread"))
    });
    assert_eq!(
        after_nested.await,
        caller,
        "the outer call is still in force"
    );
    let (other_submitter, ran_on) = elsewhere;
    assert_ne!(ran_on.await, other_submitter, "the other thread's work");

    assert_ne!(submitted_by(&pool).await, caller, "back on a pool thread");
}

/// Work that panics when it runs at its submission leaves `submit` to return, and resumes the
/// panic on its awaiter, as work on a pool thread does.
#[test]
fn panic_at_the_submission_resumes_on_awaiter() {
    let pool = SyscallPool::new(1);
    let task = run_file_operations_on_this_thread(|| pool.submit(|| panic!("deliberate")));

    let mut task = std::pin::pin!(task);
    let polled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        task.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_ready()
    }));
    assert!(polled.is_err(), "the awaiter takes the panic");
}

/// A panic in the call ends it for the thread, which submits to the pool again.
#[tokio::test]
async fn a_panic_in_the_call_leaves_the_thread_submitting_to_the_pool() {
    let pool = SyscallPool::new(1);
    let panicked =
        std::panic::catch_unwind(|| run_file_operations_on_this_thread(|| panic!("deliberate")));
    assert!(panicked.is_err());

    let caller = std::thread::current().id();
    let ran_on = pool.submit(|| std::thread::current().id()).await;
    assert_ne!(ran_on, caller, "back on a pool thread");
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
        state.push(
            queued_job(move || {
                flag.fetch_add(1, Ordering::SeqCst);
            }),
            Instant::now(),
        );
        state.running += 1;
    }

    pool.run_without_worker();

    assert_eq!(ran.load(Ordering::SeqCst), 1, "the job never ran");
    {
        let state = pool.inner.state.lock();
        assert_eq!(state.running, 0, "the reserved slot was not released");
        assert!(state.queue.is_empty());
        assert_eq!(state.spawn_refused, 1);
        assert_eq!(state.saturated, 0, "the pool was below its cap");
    }
    assert_eq!(
        pool.take_threads_high_water(),
        0,
        "a refused thread reached the window"
    );
    assert_eq!(pool.stats().threads_high_water, 0);
}

/// A live worker will drain the queue, so blocking the caller would buy nothing.
#[test]
fn a_refused_worker_thread_leaves_the_job_when_one_is_alive() {
    let pool = SyscallPool::new(3);
    let ran = Arc::new(AtomicUsize::new(0));
    let flag = Arc::clone(&ran);
    {
        let mut state = pool.inner.state.lock();
        state.push(
            queued_job(move || {
                flag.fetch_add(1, Ordering::SeqCst);
            }),
            Instant::now(),
        );
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
    assert_eq!(state.spawn_refused, 1);
    assert_eq!(state.saturated, 0, "the pool was below its cap");
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

/// Lets a job held by [`occupy_the_only_thread`] finish. Dropping it does the same, so a test
/// that fails before opening it does not leave a pool thread spinning.
struct Release(Arc<AtomicBool>);

impl Release {
    fn open(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl Drop for Release {
    fn drop(&mut self) {
        self.open();
    }
}

/// Submits a job that holds the pool's only thread until it is released, and returns once that
/// job has started, so whatever is submitted next queues behind it.
fn occupy_the_only_thread(pool: &SyscallPool) -> (Release, SyscallTask<()>) {
    let release = Arc::new(AtomicBool::new(false));
    let started = Arc::new(AtomicBool::new(false));
    let running = Arc::clone(&started);
    let gate = Arc::clone(&release);
    let blocker = pool.submit(move || {
        running.store(true, Ordering::SeqCst);
        while !gate.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
    });
    while !started.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }
    (Release(release), blocker)
}

const HOLD: Duration = Duration::from_millis(20);
const QUEUED: usize = 8;

/// Fills a one-thread pool's queue behind a job held for [`HOLD`], then lets it all run.
async fn queue_behind_a_held_job(pool: &SyscallPool) {
    let (release, blocker) = occupy_the_only_thread(pool);
    let queued: Vec<_> = (0..QUEUED)
        .map(|index| pool.submit(move || index))
        .collect();
    std::thread::sleep(HOLD);
    release.open();

    blocker.await;
    for (index, task) in queued.into_iter().enumerate() {
        assert_eq!(task.await, index);
    }
}

mod stats {
    use super::*;

    /// A job counts as started and its queue wait is recorded when it leaves the queue. A job
    /// still queued counts as submitted but not started.
    #[tokio::test]
    async fn a_queued_job_is_submitted_but_not_started() {
        let pool = SyscallPool::new(1);
        let (release, blocker) = occupy_the_only_thread(&pool);
        let queued = pool.submit(|| ());

        let stats = pool.stats();
        assert_eq!(stats.submitted, 2);
        assert_eq!(stats.started, 1);

        release.open();
        blocker.await;
        queued.await;
        assert_eq!(pool.stats().started, 2);
    }

    /// Every submission is counted, and one that finds the only thread busy is counted as
    /// saturated. Each queued job waited at least as long as the job ahead of it was held.
    #[tokio::test]
    async fn counts_submissions_saturation_and_queue_wait() {
        let pool = SyscallPool::new(1);
        queue_behind_a_held_job(&pool).await;

        let stats = pool.stats();
        assert_eq!(stats.submitted, QUEUED as u64 + 1);
        assert_eq!(stats.started, QUEUED as u64 + 1);
        assert_eq!(stats.saturated, QUEUED as u64);
        assert!(
            stats.queue_wait >= HOLD * QUEUED as u32,
            "{:?} queued in total behind a job held for {HOLD:?}",
            stats.queue_wait
        );
    }

    /// With a cap no smaller than the number of jobs, a submission can always reserve a new
    /// thread, whatever the existing ones are doing.
    #[tokio::test]
    async fn a_pool_below_its_cap_is_not_saturated() {
        let pool = SyscallPool::new(8);
        for index in 0..8 {
            assert_eq!(pool.submit(move || index).await, index);
        }

        let stats = pool.stats();
        assert_eq!(stats.submitted, 8);
        assert_eq!(stats.saturated, 0);
    }
}

mod take_queue_high_water {
    use super::*;

    #[tokio::test]
    async fn reports_the_peak_of_a_burst_that_has_drained() {
        let pool = SyscallPool::new(1);
        queue_behind_a_held_job(&pool).await;

        assert_eq!(pool.take_queue_high_water(), QUEUED);
        assert_eq!(pool.stats().queued, 0, "the burst has drained");
    }

    /// Taking the peak starts the next from the current depth, and leaves the high-water mark
    /// since creation in place.
    #[tokio::test]
    async fn starts_the_next_peak_from_the_current_depth() {
        let pool = SyscallPool::new(1);
        queue_behind_a_held_job(&pool).await;
        pool.take_queue_high_water();

        assert_eq!(pool.take_queue_high_water(), 0);
        assert_eq!(pool.stats().queue_high_water, QUEUED);
    }
}

mod take_threads_high_water {
    use super::*;

    /// A thread still alive when the peak is taken is counted in the next one, and the peak
    /// since creation is left in place.
    #[tokio::test]
    async fn starts_the_next_peak_from_the_threads_alive() {
        let pool = SyscallPool::new(1);
        let (release, blocker) = occupy_the_only_thread(&pool);

        assert_eq!(pool.take_threads_high_water(), 1);
        assert_eq!(
            pool.take_threads_high_water(),
            1,
            "the busy thread dropped out"
        );
        release.open();
        blocker.await;
        assert_eq!(pool.stats().threads_high_water, 1);
    }

    /// A reserved slot does not count as a thread, in the peaks or the current count, until the
    /// thread starts.
    #[test]
    fn counts_only_threads_that_have_started() {
        let pool = SyscallPool::new(2);
        pool.inner.state.lock().running += 1;

        assert_eq!(pool.take_threads_high_water(), 0);
        let stats = pool.stats();
        assert_eq!(stats.threads_high_water, 0);
        assert_eq!(stats.threads, 0);
        assert_eq!(stats.executing, 0);
    }
}

mod take_max_queue_wait {
    use super::*;

    #[tokio::test]
    async fn reports_the_longest_wait_of_a_burst_that_has_drained() {
        let pool = SyscallPool::new(1);
        queue_behind_a_held_job(&pool).await;

        let longest = pool.take_max_queue_wait();
        assert!(
            longest >= HOLD,
            "{longest:?} at most behind a job held for {HOLD:?}"
        );
        assert_eq!(pool.take_max_queue_wait(), Duration::ZERO);
    }

    /// A pool whose only thread is blocked reports the wait of the job queued behind it, though
    /// that job has not started.
    #[tokio::test]
    async fn counts_a_job_that_is_still_queued() {
        let pool = SyscallPool::new(1);
        let (release, blocker) = occupy_the_only_thread(&pool);
        let queued = pool.submit(|| ());
        std::thread::sleep(HOLD);

        let longest = pool.take_max_queue_wait();
        release.open();
        blocker.await;
        queued.await;

        assert_eq!(pool.stats().started, 2);
        assert!(
            longest >= HOLD,
            "{longest:?} reported for a job queued for {HOLD:?}"
        );
    }
}

/// The pool lends at most half its cap in all, and starts no more threads than its cap less what
/// it lent, while work keeps completing on the rest.
#[tokio::test]
async fn a_pool_lends_at_most_half_its_cap_and_keeps_the_rest_running() {
    let pool = SyscallPool::new(8);
    let first = pool.lend(3);
    let second = pool.lend(100);
    let third = pool.lend(100);
    assert_eq!((first.count(), second.count(), third.count()), (3, 1, 0));

    let (release, released) = std::sync::mpsc::channel::<()>();
    let released = Arc::new(Mutex::new(released));
    let blocked: Vec<_> = (0..8)
        .map(|_| {
            let released = Arc::clone(&released);
            pool.submit(move || {
                let _ = released.lock().recv();
            })
        })
        .collect();
    tokio::time::timeout(Duration::from_secs(5), async {
        while pool.stats().executing < 4 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the pool runs its share");
    assert_eq!(pool.stats().threads, 4, "the cap less what is lent");

    for _ in 0..8 {
        release.send(()).expect("release a job");
    }
    for task in blocked {
        task.await;
    }
    drop((first, second, third));
    assert_eq!(pool.lend(100).count(), 4, "what was lent comes back");
}

/// Idle threads past the cap less what is lent end at once, rather than at their keep-alive.
#[tokio::test]
async fn idle_threads_past_the_cap_end_when_threads_are_lent() {
    let pool = SyscallPool::new(8);
    let (release, released) = std::sync::mpsc::channel::<()>();
    let released = Arc::new(Mutex::new(released));
    let blocked: Vec<_> = (0..8)
        .map(|_| {
            let released = Arc::clone(&released);
            pool.submit(move || {
                let _ = released.lock().recv();
            })
        })
        .collect();
    tokio::time::timeout(Duration::from_secs(5), async {
        while pool.stats().executing < 8 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("eight threads run");
    for _ in 0..8 {
        release.send(()).expect("release a job");
    }
    for task in blocked {
        task.await;
    }
    // A thread hands its job's result over before it counts itself idle.
    tokio::time::timeout(Duration::from_secs(5), async {
        while pool.stats().executing > 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the threads go idle");

    let lent = pool.lend(4);

    assert_eq!(lent.count(), 4);
    tokio::time::timeout(Duration::from_secs(5), async {
        while pool.stats().threads > 4 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the threads past the cap end");
}

/// Threads busy with work are not lent.
#[tokio::test]
async fn busy_threads_are_not_lent() {
    let pool = SyscallPool::new(8);
    let (release, released) = std::sync::mpsc::channel::<()>();
    let released = Arc::new(Mutex::new(released));
    let blocked: Vec<_> = (0..6)
        .map(|_| {
            let released = Arc::clone(&released);
            pool.submit(move || {
                let _ = released.lock().recv();
            })
        })
        .collect();
    tokio::time::timeout(Duration::from_secs(5), async {
        while pool.stats().executing < 6 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("six threads run");

    assert_eq!(pool.lend(100).count(), 2);

    for _ in 0..6 {
        release.send(()).expect("release a job");
    }
    for task in blocked {
        task.await;
    }
}

/// Jobs that queued while threads were lent run on the whole cap once the threads come back,
/// with no later submission needed to start a thread for them.
#[tokio::test]
async fn jobs_queued_while_threads_were_lent_get_the_threads_back() {
    let pool = SyscallPool::new(8);
    let lent = pool.lend(4);
    let (release, released) = std::sync::mpsc::channel::<()>();
    let released = Arc::new(Mutex::new(released));
    let blocked: Vec<_> = (0..8)
        .map(|_| {
            let released = Arc::clone(&released);
            pool.submit(move || {
                let _ = released.lock().recv();
            })
        })
        .collect();
    tokio::time::timeout(Duration::from_secs(5), async {
        while pool.stats().queued > 4 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the pool runs its share");
    let stats = pool.stats();
    assert_eq!(
        (stats.threads, stats.queued),
        (4, 4),
        "the rest wait for threads"
    );

    drop(lent);

    tokio::time::timeout(Duration::from_secs(5), async {
        while pool.stats().executing < 8 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the queued jobs run on the threads given back");
    for _ in 0..8 {
        release.send(()).expect("release a job");
    }
    for task in blocked {
        task.await;
    }
}

/// A refusal among threads reserved together hands back every slot not started and runs a job
/// where no worker is alive, as a single refused thread does.
#[test]
fn refused_threads_reserved_together_release_their_slots() {
    let pool = SyscallPool::new(4);
    let ran = Arc::new(AtomicUsize::new(0));
    let flag = Arc::clone(&ran);
    {
        let mut state = pool.inner.state.lock();
        state.push(
            queued_job(move || {
                flag.fetch_add(1, Ordering::SeqCst);
            }),
            Instant::now(),
        );
        state.running += 3;
    }

    pool.run_without_workers(3);

    assert_eq!(ran.load(Ordering::SeqCst), 1, "the job never ran");
    assert_eq!(
        pool.inner.state.lock().running,
        0,
        "slots were not released"
    );
}

thread_local! {
    /// Times the thread ran the begin hook and the end hook, and whether the thread is idle: it ran
    /// the begin hook and no end hook since.
    static IDLE_BEGINS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static IDLE_ENDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static IDLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Whether the thread runs the jobs of [`LOCK_TEST_POOL`], and whether its last begin hook
    /// found that pool's lock free.
    static IN_LOCK_TEST_POOL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static BEGIN_FOUND_LOCK_FREE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The pool of the test that the begin hook runs without the pool's lock held.
static LOCK_TEST_POOL: std::sync::OnceLock<SyscallPool> = std::sync::OnceLock::new();

/// Set by the begin hook on the thread of [`LOCK_TEST_POOL`].
static LOCK_TEST_BEGUN: AtomicBool = AtomicBool::new(false);

fn record_idle_begin() {
    IDLE_BEGINS.set(IDLE_BEGINS.get() + 1);
    IDLE.set(true);
    if IN_LOCK_TEST_POOL.get() {
        let pool = LOCK_TEST_POOL.get().expect("the pool of the lock test");
        BEGIN_FOUND_LOCK_FREE.set(pool.inner.state.try_lock().is_some());
        LOCK_TEST_BEGUN.store(true, Ordering::Release);
    }
}

fn record_idle_end() {
    IDLE_ENDS.set(IDLE_ENDS.get() + 1);
    IDLE.set(false);
}

/// Sets the hooks that record their calls on the thread, once for the tests that read them: the
/// hooks are set once per process.
fn set_recording_idle_hooks() {
    static SET: std::sync::Once = std::sync::Once::new();
    SET.call_once(|| {
        assert!(
            set_idle_hooks(IdleHooks {
                begin: record_idle_begin,
                end: record_idle_end,
            }),
            "hooks set by another test"
        );
    });
}

/// A pool thread runs the begin hook as it waits for work, and the end hook once it has work,
/// before the work runs.
#[tokio::test]
async fn a_pool_thread_runs_the_idle_hooks_around_its_wait() {
    set_recording_idle_hooks();
    let pool = SyscallPool::new(1);
    let first = pool
        .submit(|| (std::thread::current().id(), IDLE_BEGINS.get()))
        .await;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while pool.inner.state.lock().idle == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the pool thread never waited"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let second = pool
        .submit(|| (std::thread::current().id(), IDLE_BEGINS.get(), IDLE.get()))
        .await;
    assert_eq!(first.0, second.0, "one thread ran both");
    assert!(
        second.1 > first.1,
        "the begin hook did not run as it waited"
    );
    assert!(!second.2, "the end hook did not run before the work");
}

/// A pool thread runs the begin hook without the pool's lock held, which every submitter takes.
#[tokio::test]
async fn a_pool_thread_runs_the_begin_hook_without_the_pool_lock() {
    set_recording_idle_hooks();
    let pool = LOCK_TEST_POOL.get_or_init(|| SyscallPool::new(1));
    pool.submit(|| IN_LOCK_TEST_POOL.set(true)).await;
    // Polls a flag the hook sets rather than the pool's state, whose lock the hook would find held.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !LOCK_TEST_BEGUN.load(Ordering::Acquire) {
        assert!(
            std::time::Instant::now() < deadline,
            "the pool thread never waited"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(
        pool.submit(|| BEGIN_FOUND_LOCK_FREE.get()).await,
        "the begin hook ran with the pool's lock held"
    );
}

/// A pool thread that finds work queued as it ends a job takes the work without waiting, and runs
/// neither hook.
#[tokio::test]
async fn a_pool_thread_taking_queued_work_runs_no_idle_hook() {
    set_recording_idle_hooks();
    let pool = SyscallPool::new(1);
    let (release, released) = std::sync::mpsc::channel::<()>();
    let first = pool.submit(move || {
        released.recv().expect("the release");
        (
            std::thread::current().id(),
            IDLE_BEGINS.get(),
            IDLE_ENDS.get(),
        )
    });
    let second = pool.submit(|| {
        (
            std::thread::current().id(),
            IDLE_BEGINS.get(),
            IDLE_ENDS.get(),
        )
    });
    release.send(()).expect("the first job waits");
    let (first, second) = (first.await, second.await);
    assert_eq!(first.0, second.0, "one thread ran both");
    assert_eq!(
        (second.1, second.2),
        (first.1, first.2),
        "the thread ran an idle hook between queued jobs"
    );
}
