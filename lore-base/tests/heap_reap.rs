// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Reaps of the heaps of threads waiting for work, which need their own test binary: a reap
//! collects the idle heaps of every thread of the process, so the count it returns is exact only
//! where no other test's threads wait for work. Nothing here builds the process-wide runtimes,
//! which start the reap thread that would race the test's reaps.
use std::hint::black_box;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use lore_base::allocator::reap::heap_reap_enabled;
use lore_base::allocator::reap::reap_idle_heaps_now;
use lore_base::allocator::reap::thread_idle_begin;
use lore_base::allocator::reap::thread_idle_end;
use lore_base::runtime::CoreTimer;
use lore_base::runtime::build_core_runtime;
use lore_base::runtime::build_net_runtime;

/// Allocates and frees blocks over several pages, so the heap holds free pages to reap.
fn use_heap() {
    let blocks: Vec<Vec<u8>> = (0..64).map(|_| vec![1; 64 * 1024]).collect();
    drop(black_box(blocks));
}

/// Waits until `step` reaches `value`. Polls rather than blocking on a channel, which would
/// allocate and so take back the heap of a thread waiting with its heap idle.
fn wait_for_step(step: &AtomicU32, value: u32) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while step.load(Ordering::Acquire) < value {
        assert!(Instant::now() < deadline, "step {value} not reached");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Waits until a reap collects a heap, failing with `failure` if none has within 30 s.
fn wait_for_reap(failure: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while reap_idle_heaps_now(0) == 0 {
        assert!(Instant::now() < deadline, "{failure}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits until reaps over a tenth of a second find no heap to reap, so that the threads started so
/// far have used their heaps, waited and been reaped, and a later reap collects only the heaps of
/// threads used after this.
fn wait_for_no_reap() {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut quiet = 0;
    while quiet < 10 {
        quiet = if reap_idle_heaps_now(0) == 0 {
            quiet + 1
        } else {
            0
        };
        assert!(Instant::now() < deadline, "heaps kept being reaped");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A thread's heap left idle after use is reaped once, and again only after the thread has used
/// it again. The calling thread's heap is not counted.
fn an_idle_heap_is_reaped_once_per_use() {
    static STEP: AtomicU32 = AtomicU32::new(0);
    assert_eq!(reap_idle_heaps_now(0), 0, "no heap is idle yet");
    let thread = std::thread::spawn(|| {
        for round in 0..2 {
            use_heap();
            thread_idle_begin();
            STEP.store(round * 2 + 1, Ordering::Release);
            wait_for_step(&STEP, round * 2 + 2);
            thread_idle_end();
        }
    });
    for round in 0..2 {
        wait_for_step(&STEP, round * 2 + 1);
        assert_eq!(reap_idle_heaps_now(0), 1, "round {round}: the idle heap");
        assert_eq!(
            reap_idle_heaps_now(0),
            0,
            "round {round}: the heap reaped and unused since"
        );
        STEP.store(round * 2 + 2, Ordering::Release);
    }
    thread.join().expect("the idle thread");
}

/// A thread ending with its heap idle, as a file I/O pool thread does when no work comes within its
/// keep-alive, releases the heap rather than leaving it to the reaps, and a later thread allocates
/// as usual.
fn a_thread_ending_with_its_heap_idle_releases_it() {
    std::thread::spawn(|| {
        use_heap();
        thread_idle_begin();
    })
    .join()
    .expect("the thread ending with its heap idle");
    assert_eq!(
        reap_idle_heaps_now(0),
        0,
        "the heap of the ended thread left idle"
    );
    std::thread::spawn(use_heap)
        .join()
        .expect("a thread after the release");
}

/// The workers of a core runtime leave their heaps idle as they park, and so do the file I/O pool
/// threads the runtime sets up as they wait for work.
fn core_workers_and_file_io_threads_are_reaped(runtime: &tokio::runtime::Runtime) {
    runtime.block_on(async {
        lore_base::lore_spawn!(async { use_heap() })
            .await
            .expect("the task");
    });
    wait_for_reap("no parked core worker heap was reaped");
    wait_for_no_reap();
    // Listed from this thread, so the pool thread is the only one that uses its heap.
    runtime.block_on(async {
        let mut entries = lore_io::IoDriver::global()
            .read_dir(env!("CARGO_MANIFEST_DIR"))
            .await
            .expect("the listing");
        while let Some(entry) = entries.next().await {
            entry.expect("an entry");
        }
    });
    wait_for_reap("no heap of a file I/O pool thread waiting for work was reaped");
}

/// The workers of the net runtime leave their heaps idle as they park.
fn net_workers_are_reaped() {
    let net = build_net_runtime();
    wait_for_no_reap();
    // Spawned on the runtime built here rather than through `lore_spawn_net!`, whose process-wide
    // net runtime would start the reap thread.
    #[allow(clippy::disallowed_methods)]
    let task = net.spawn(async { use_heap() });
    net.block_on(task).expect("the task");
    wait_for_reap("no parked net worker heap was reaped");
}

/// The name of [`idle_heaps_are_reaped`], for running it again in a child process.
const REAP_TEST: &str = "idle_heaps_are_reaped";

/// One test, so that its parts run one after the other: the count each reap returns depends on
/// which threads of the process are waiting for work.
#[test]
fn idle_heaps_are_reaped() {
    if !heap_reap_enabled() {
        // Under another global allocator no thread leaves a heap idle, and a reap does nothing.
        assert_eq!(reap_idle_heaps_now(0), 0);
        return;
    }
    an_idle_heap_is_reaped_once_per_use();
    a_thread_ending_with_its_heap_idle_releases_it();
    let runtime = build_core_runtime(None, CoreTimer::Shared);
    core_workers_and_file_io_threads_are_reaped(&runtime);
    net_workers_are_reaped();
}

/// Under the system allocator nothing is reaped: the test above passes in a child process run with
/// `LORE_ALLOCATOR=system`, which takes its first branch.
#[test]
fn no_heap_is_reaped_under_the_system_allocator() {
    if std::env::var_os("LORE_ALLOCATOR").is_some() {
        return;
    }
    let status = std::process::Command::new(std::env::current_exe().expect("this test binary"))
        .args(["--exact", REAP_TEST])
        .env("LORE_ALLOCATOR", "system")
        .status()
        .expect("run the child");
    assert!(
        status.success(),
        "the reap test failed under the system allocator"
    );
}
