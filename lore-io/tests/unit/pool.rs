// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Waker;
use std::time::Duration;

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
