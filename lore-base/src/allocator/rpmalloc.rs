// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::alloc::GlobalAlloc;
use std::alloc::Layout;
use std::sync::atomic::Ordering;

use super::TRACKING_ALLOCATIONS;
use super::tracking;

#[repr(C)]
#[derive(Default)]
pub struct RpmallocHeapStatistics {
    pub allocated_bytes: usize,
    pub committed_bytes: usize,
    pub mapped_bytes: usize,
}

#[repr(C)]
#[derive(Default)]
pub struct RpmallocGlobalStatistics {
    pub mapped: usize,
    pub mapped_peak: usize,
    pub committed: usize,
    pub decommitted: usize,
    pub active: usize,
    pub active_peak: usize,
    pub huge_alloc: usize,
    pub huge_alloc_peak: usize,
    pub heap_count: usize,
}

#[allow(unused)]
unsafe extern "C" {
    fn rpmalloc(size: usize) -> *mut std::ffi::c_void;
    fn rpzalloc(size: usize) -> *mut std::ffi::c_void;
    fn rpaligned_alloc(align: usize, size: usize) -> *mut std::ffi::c_void;
    fn rpaligned_zalloc(align: usize, size: usize) -> *mut std::ffi::c_void;
    fn rpaligned_realloc(
        ptr: *mut std::ffi::c_void,
        align: usize,
        new_size: usize,
        old_size: usize,
        flags: u32,
    ) -> *mut std::ffi::c_void;
    fn rpfree(ptr: *mut std::ffi::c_void);
    fn rpmalloc_heap_acquire() -> *mut std::ffi::c_void;
    fn rpmalloc_heap_release(heap: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    fn rpmalloc_heap_aligned_alloc(
        heap: *mut std::ffi::c_void,
        align: usize,
        size: usize,
    ) -> *mut std::ffi::c_void;
    fn rpmalloc_heap_aligned_zalloc(
        heap: *mut std::ffi::c_void,
        align: usize,
        size: usize,
    ) -> *mut std::ffi::c_void;
    fn rpmalloc_heap_aligned_realloc(
        heap: *mut std::ffi::c_void,
        ptr: *mut std::ffi::c_void,
        align: usize,
        new_size: usize,
        old_size: usize,
        flags: u32,
    ) -> *mut std::ffi::c_void;
    fn rpmalloc_heap_free(heap: *mut std::ffi::c_void, ptr: *mut std::ffi::c_void);
    pub(crate) fn rpmalloc_heap_statistics(heap: *mut std::ffi::c_void) -> RpmallocHeapStatistics;
    #[link_name = "rpmalloc_global_statistics"]
    pub(crate) fn rpmalloc_global_statistics_ffi(stats: *mut RpmallocGlobalStatistics);
}

pub(crate) struct RpmallocAllocator;

unsafe impl GlobalAlloc for RpmallocAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe {
            if layout.align() <= 16 {
                rpmalloc(layout.size()).cast::<u8>()
            } else {
                rpaligned_alloc(layout.align(), layout.size()).cast::<u8>()
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        unsafe {
            rpfree(ptr.cast::<std::ffi::c_void>());
        }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        unsafe {
            if layout.align() <= 16 {
                rpzalloc(layout.size()).cast::<u8>()
            } else {
                rpaligned_zalloc(layout.align(), layout.size()).cast::<u8>()
            }
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        unsafe {
            rpaligned_realloc(
                ptr.cast::<std::ffi::c_void>(),
                layout.align(),
                new_size,
                0,
                0,
            )
            .cast::<u8>()
        }
    }
}

pub(crate) static RPMALLOC_ALLOCATOR: RpmallocAllocator = RpmallocAllocator;

#[derive(Default)]
pub struct RpmallocHeapAllocator {
    pub(crate) heap: parking_lot::Mutex<*mut std::ffi::c_void>,
}

unsafe impl Send for RpmallocHeapAllocator {}
unsafe impl Sync for RpmallocHeapAllocator {}

impl Drop for RpmallocHeapAllocator {
    fn drop(&mut self) {
        let mut heap = self.heap.lock();
        if !heap.is_null() {
            unsafe {
                rpmalloc_heap_release(*heap);
            }
            *heap = std::ptr::null_mut();
        }
    }
}

unsafe impl GlobalAlloc for RpmallocHeapAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe {
            let mut heap = self.heap.lock();
            if heap.is_null() {
                *heap = rpmalloc_heap_acquire();
            }
            rpmalloc_heap_aligned_alloc(*heap, layout.align(), layout.size())
        };
        if TRACKING_ALLOCATIONS.load(Ordering::Relaxed) && !ptr.is_null() {
            tracking::track_alloc(ptr.cast(), layout.size());
        }
        ptr.cast()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        if TRACKING_ALLOCATIONS.load(Ordering::Relaxed) && !ptr.is_null() {
            tracking::track_dealloc(ptr);
        }
        unsafe {
            let heap = self.heap.lock();
            if !heap.is_null() {
                rpmalloc_heap_free(*heap, ptr.cast::<std::ffi::c_void>());
            }
        }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe {
            let mut heap = self.heap.lock();
            if heap.is_null() {
                *heap = rpmalloc_heap_acquire();
            }
            rpmalloc_heap_aligned_zalloc(*heap, layout.align(), layout.size())
        };
        if TRACKING_ALLOCATIONS.load(Ordering::Relaxed) && !ptr.is_null() {
            tracking::track_alloc(ptr.cast(), layout.size());
        }
        ptr.cast()
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe {
            let mut heap = self.heap.lock();
            if heap.is_null() {
                *heap = rpmalloc_heap_acquire();
            }
            rpmalloc_heap_aligned_realloc(
                *heap,
                ptr.cast::<std::ffi::c_void>(),
                layout.align(),
                new_size,
                0,
                0,
            )
        };
        if TRACKING_ALLOCATIONS.load(Ordering::Relaxed) {
            tracking::track_realloc(ptr, new_ptr.cast(), layout.size());
        }
        new_ptr.cast()
    }
}
