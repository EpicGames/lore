// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::alloc::GlobalAlloc;
use std::alloc::Layout;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use lore_base::allocator::GrowVec;
use lore_base::allocator::RpmallocHeapAllocator;
use lore_base::allocator::on_next_heap_lock_wait;
use lore_base::allocator::set_allocation_counting;
use lore_base::allocator::thread_allocation_count;

/// Serializes the tests in this module: they all read/write the same process-wide
/// counting state and must not observe each other's `set_allocation_counting` calls.
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Restores counting to disabled on drop, including on test panic/unwind.
struct DisableCountingOnDrop;
impl Drop for DisableCountingOnDrop {
    fn drop(&mut self) {
        set_allocation_counting(false);
    }
}

#[test]
fn disabled_counting_leaves_counts_unchanged() {
    let _lock = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    set_allocation_counting(false);
    let before = thread_allocation_count();
    let boxed = Box::new(7u64);
    let _ = std::hint::black_box(&boxed);
    drop(boxed);
    assert_eq!(
        thread_allocation_count(),
        before,
        "counting must be a no-op while disabled"
    );
}

#[test]
fn thread_count_increments_for_box_new_when_enabled() {
    let _lock = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    set_allocation_counting(true);
    let _restore = DisableCountingOnDrop;
    let before = thread_allocation_count();
    let boxed = Box::new(7u64);
    let _ = std::hint::black_box(&boxed);
    assert!(
        thread_allocation_count() > before,
        "Box::new must be counted while enabled"
    );
}

#[test]
fn realloc_is_counted_once() {
    let _lock = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    set_allocation_counting(true);
    let _restore = DisableCountingOnDrop;
    let mut values = Vec::<u64>::with_capacity(1);
    let before = thread_allocation_count();
    values.reserve_exact(64);
    let _ = std::hint::black_box(&values);
    assert_eq!(thread_allocation_count() - before, 1);
}

#[test]
fn private_heap_allocation_is_counted_once() {
    let _lock = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    set_allocation_counting(true);
    let _restore = DisableCountingOnDrop;
    let heap = RpmallocHeapAllocator::default();
    let layout = Layout::new::<u64>();
    let before = thread_allocation_count();
    unsafe {
        let ptr = heap.alloc(layout);
        assert!(!ptr.is_null());
        heap.dealloc(ptr, layout);
    }
    assert_eq!(thread_allocation_count() - before, 1);
}

#[test]
fn dealloc_is_not_counted() {
    let _lock = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    set_allocation_counting(true);
    let _restore = DisableCountingOnDrop;
    let before = thread_allocation_count();
    {
        let boxed = Box::new(7u64);
        let _ = std::hint::black_box(&boxed);
    }
    let after = thread_allocation_count();
    assert_eq!(
        after - before,
        1,
        "dropping the box must not add a second count for its dealloc"
    );
}

/// Removals from a `GrowVec`, shifting, swapping and down to empty, allocate nothing.
#[test]
fn growvec_removals_allocate_nothing() {
    let _lock = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let mut values: GrowVec<u32, 8> = GrowVec::new();
    for value in 0..40 {
        values.push(value);
    }
    set_allocation_counting(true);
    let _restore = DisableCountingOnDrop;
    let before = thread_allocation_count();
    values.remove(0);
    values.remove(17);
    values.swap_remove(3);
    while !values.is_empty() {
        values.swap_remove(0);
    }
    let _ = std::hint::black_box(&values);
    assert_eq!(thread_allocation_count() - before, 0);
}

/// An allocation from a private heap that waits for the heap's lock is counted once. The callback
/// allocates inside the wait as parking the thread does, and the holder releases the lock only once
/// the callback has run, so the allocation always takes the waiting path.
#[test]
fn private_heap_allocation_that_waits_for_the_lock_is_counted_once() {
    let _lock = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let heap = RpmallocHeapAllocator::default();
    let layout = Layout::new::<u64>();
    let (locked_tx, locked_rx) = mpsc::channel();
    let (waiting_tx, waiting_rx) = mpsc::channel();
    let waited = Arc::new(AtomicBool::new(false));
    let shared = &heap;
    thread::scope(|scope| {
        scope.spawn(move || {
            let _heap = shared.heap.lock();
            locked_tx
                .send(())
                .expect("the measuring thread receives the locked signal");
            // Bounded, so a lock that skips the wait fails the assertion on `waited` rather than
            // hanging the binary.
            let _ = waiting_rx.recv_timeout(Duration::from_secs(30));
        });
        locked_rx.recv().expect("the holder takes the lock");
        let in_wait = waited.clone();
        on_next_heap_lock_wait(move || {
            let _ = std::hint::black_box(Box::new(7u64));
            in_wait.store(true, Ordering::Relaxed);
            let _ = waiting_tx.send(());
        });
        set_allocation_counting(true);
        let _restore = DisableCountingOnDrop;
        let before = thread_allocation_count();
        let ptr = unsafe { heap.alloc(layout) };
        let counted = thread_allocation_count() - before;
        assert!(!ptr.is_null());
        unsafe { heap.dealloc(ptr, layout) };
        assert!(
            waited.load(Ordering::Relaxed),
            "the allocation waits for the lock"
        );
        assert_eq!(counted, 1);
    });
}
