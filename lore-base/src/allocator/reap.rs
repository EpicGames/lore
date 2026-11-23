// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::Mutex;

use super::rpmalloc::rpmalloc_reap_idle_heaps;
use super::rpmalloc::rpmalloc_thread_idle_begin;
use super::rpmalloc::rpmalloc_thread_idle_end;

/// How long a heap is idle before a reap collects it, in milliseconds, and the wait between reaps
/// while they find heaps to reap.
#[lore_macro::test_pub]
const REAP_MIN_IDLE_MS: u32 = 1000;

/// The longest wait between reaps. The wait doubles from the minimum idle time while the reaps find
/// no heap to reap.
#[lore_macro::test_pub]
const REAP_WAIT_MAX: Duration = Duration::from_secs(8);

/// The most heaps one call into rpmalloc reaps, which is also rpmalloc's own cap. A call that
/// reaps fewer has reaped every heap due.
const REAP_LIMIT: u32 = 64;

/// The reap thread, from its start until [`stop_heap_reap`] takes it.
static REAP_THREAD: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

/// Set by [`stop_heap_reap`], after which the reap thread ends and is not started again.
static REAP_STOPPED: AtomicBool = AtomicBool::new(false);

/// Whether the reap thread runs: set before it starts, and cleared as it ends.
#[cfg(feature = "test-util")]
static REAP_THREAD_LIVE: AtomicBool = AtomicBool::new(false);

/// Whether threads leave their heap idle while they wait for work, and a thread reaps the idle
/// heaps: where rpmalloc is the global allocator.
#[lore_macro::test_pub]
pub(crate) fn heap_reap_enabled() -> bool {
    super::rpmalloc_selected()
}

/// Leaves the calling thread's rpmalloc heap idle as the thread begins waiting for work, so a reap
/// can collect the memory the heap holds without using it. The thread takes the same heap back in
/// [`thread_idle_end`], or with its first allocation before that. Only for where
/// [`heap_reap_enabled`].
#[lore_macro::test_pub]
pub(crate) fn thread_idle_begin() {
    // SAFETY: acts on the calling thread's heap only, and does nothing for a thread without one.
    unsafe { rpmalloc_thread_idle_begin() };
}

/// Takes back the rpmalloc heap the calling thread left idle in [`thread_idle_begin`], waiting for
/// a reap of the heap to end. Only for where [`heap_reap_enabled`].
#[lore_macro::test_pub]
pub(crate) fn thread_idle_end() {
    // SAFETY: acts on the calling thread's heap only, and does nothing for a thread without one.
    unsafe { rpmalloc_thread_idle_end() };
}

/// Starts the thread that reaps the idle rpmalloc heaps, where [`heap_reap_enabled`], unless it
/// runs or was stopped. Each runtime whose threads leave their heaps idle starts it.
///
/// The thread is outside the runtime pools and their thread budget, as it sleeps but for the reaps:
/// the decommits of a reap after a large operation would hold a pool thread from its work, and the
/// core runtime's blocking pool has two threads by default. Where the system refuses the thread,
/// the heaps are not reaped.
pub(crate) fn start_heap_reap() {
    if !heap_reap_enabled() {
        return;
    }
    let mut thread = REAP_THREAD.lock();
    if thread.is_some() || REAP_STOPPED.load(Ordering::Acquire) {
        return;
    }
    #[cfg(feature = "test-util")]
    REAP_THREAD_LIVE.store(true, Ordering::Release);
    match std::thread::Builder::new()
        .name("lore-heap-reap".to_string())
        .spawn(reap_idle_heaps)
    {
        Ok(handle) => *thread = Some(handle),
        Err(error) => {
            #[cfg(feature = "test-util")]
            REAP_THREAD_LIVE.store(false, Ordering::Release);
            crate::lore_warn!(
                "The heap reap thread failed to start, so idle threads keep their allocator memory: {error}"
            );
        }
    }
}

/// Stops the reap thread for good and waits for it to end, including a reap it is running, so that
/// rpmalloc can be finalized after it.
pub(crate) fn stop_heap_reap() {
    let thread = {
        let mut thread = REAP_THREAD.lock();
        REAP_STOPPED.store(true, Ordering::Release);
        thread.take()
    };
    if let Some(thread) = thread {
        thread.thread().unpark();
        if thread.join().is_err() {
            crate::lore_warn!("The heap reap thread panicked");
        }
    }
}

/// Whether the reap thread runs, until its last statement.
#[cfg(feature = "test-util")]
pub fn heap_reap_running() -> bool {
    REAP_THREAD_LIVE.load(Ordering::Acquire)
}

/// Reaps the heaps idle for [`REAP_MIN_IDLE_MS`] and used since their last reap, after each wait
/// [`next_reap_wait`] gives, from the minimum idle time, until [`stop_heap_reap`].
fn reap_idle_heaps() {
    let mut wait = Duration::from_millis(u64::from(REAP_MIN_IDLE_MS));
    loop {
        // An early wake reaps early, which reaps only the heaps already due.
        std::thread::park_timeout(wait);
        if REAP_STOPPED.load(Ordering::Acquire) {
            #[cfg(feature = "test-util")]
            REAP_THREAD_LIVE.store(false, Ordering::Release);
            return;
        }
        wait = next_reap_wait(wait, reap_idle_heaps_now(REAP_MIN_IDLE_MS));
    }
}

/// The wait before the next reap, after a wait of `wait` and a reap that reaped `reaped` heaps: the
/// minimum idle time after a reap that found heaps to reap, as more may follow, and else twice
/// `wait`, up to [`REAP_WAIT_MAX`].
#[lore_macro::test_pub]
fn next_reap_wait(wait: Duration, reaped: usize) -> Duration {
    if reaped > 0 {
        Duration::from_millis(u64::from(REAP_MIN_IDLE_MS))
    } else {
        (wait * 2).min(REAP_WAIT_MAX)
    }
}

/// Reaps the heaps idle for at least `min_idle_ms` and used since their last reap, and returns how
/// many it reaped. Each call into rpmalloc reaps up to [`REAP_LIMIT`] heaps and holds one at a
/// time, so a thread taking its heap back waits for the reap of its own heap at most. Reaps
/// nothing unless [`heap_reap_enabled`].
#[lore_macro::test_pub]
fn reap_idle_heaps_now(min_idle_ms: u32) -> usize {
    if !heap_reap_enabled() {
        return 0;
    }
    let mut reaped = 0;
    loop {
        // SAFETY: rpmalloc is the global allocator, initialized by the first allocation.
        let count = unsafe { rpmalloc_reap_idle_heaps(min_idle_ms, REAP_LIMIT) };
        reaped += count;
        if count < REAP_LIMIT as usize {
            return reaped;
        }
    }
}
