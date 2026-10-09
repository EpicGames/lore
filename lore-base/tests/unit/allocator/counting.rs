// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::alloc::GlobalAlloc;
use std::alloc::Layout;
use std::sync::Mutex;
use std::sync::PoisonError;

use lore_base::allocator::RpmallocHeapAllocator;
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
