// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
mod growvec;
mod heapbox;
mod heapbuf;
mod rpmalloc;
mod tracking;

use std::alloc::GlobalAlloc;
use std::alloc::Layout;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

pub use growvec::GrowChunk;
pub use growvec::GrowIter;
pub use growvec::GrowIterMut;
pub use growvec::GrowVec;
pub use growvec::GrowVecMemoryStats;
pub use growvec::memory_stats;
pub use heapbox::HeapBox;
pub use heapbuf::HeapBuf;
pub use rpmalloc::RpmallocGlobalStatistics;
pub use rpmalloc::RpmallocHeapAllocator;
pub use rpmalloc::RpmallocHeapStatistics;

/// Function that allocates a block of memory with the given alignment and size.
pub type LoreAllocFn = unsafe extern "C" fn(align: usize, size: usize) -> *mut std::ffi::c_void;
/// Function that allocates a zeroed block of memory with the given alignment and size.
pub type LoreAllocZeroedFn =
    unsafe extern "C" fn(align: usize, size: usize) -> *mut std::ffi::c_void;
/// Function that resizes an existing block of memory to a new size.
pub type LoreReallocFn = unsafe extern "C" fn(
    ptr: *mut std::ffi::c_void,
    align: usize,
    size: usize,
) -> *mut std::ffi::c_void;
/// Function that frees a previously allocated block of memory.
pub type LoreDeallocFn = unsafe extern "C" fn(ptr: *mut std::ffi::c_void);

/// Set of memory functions supplied by the caller for the library to use.
pub struct ExternalAllocator {
    /// Allocation function.
    pub alloc: LoreAllocFn,
    /// Zeroed allocation function.
    pub alloc_zeroed: LoreAllocZeroedFn,
    /// Reallocation function.
    pub realloc: LoreReallocFn,
    /// Free function.
    pub dealloc: LoreDeallocFn,
}

unsafe impl GlobalAlloc for ExternalAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { (self.alloc)(layout.align(), layout.size()).cast::<u8>() }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        unsafe { (self.dealloc)(ptr.cast::<std::ffi::c_void>()) };
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        unsafe { (self.alloc_zeroed)(layout.align(), layout.size()).cast::<u8>() }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        unsafe {
            (self.realloc)(ptr.cast::<std::ffi::c_void>(), layout.align(), new_size).cast::<u8>()
        }
    }
}

static EXTERN_ALLOCATOR: OnceLock<ExternalAllocator> = OnceLock::new();
static SELECTED_ALLOCATOR: OnceLock<&'static (dyn GlobalAlloc + Sync)> = OnceLock::new();
static STANDARD_ALLOCATOR: std::alloc::System = std::alloc::System;
/// The allocation tracking options in effect, any combination of [`tracking::COUNT`],
/// [`tracking::TRACK`] and [`MEASURE`]. `tracking` in `LORE_ALLOCATOR` enables [`tracking::TRACK`]
/// from the first allocation through the global allocator, unless an external allocator was set
/// before it.
///
/// The global allocator's paths test the whole word before any one option, so with none enabled an
/// allocation, a free and a reallocation each spend one branch on tracking, whatever options exist.
static TRACKING_ALLOCATIONS: AtomicU8 = AtomicU8::new(0);

/// Tracking bit for counting the bytes held and their peak, which `LORE_MEMORY_STATS` sets. Counts
/// what callers request through the global allocator, whichever allocator serves it, so it reports
/// under rpmalloc as well, whose own statistics are compiled out of release builds. The `GrowVec`
/// and node block heaps are allocated from directly, and [`memory_summary`] reports them apart.
const MEASURE: u8 = 4;
static LIVE_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static PEAK_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[inline]
fn measure_add(size: usize) {
    let live = LIVE_BYTES.fetch_add(size, Ordering::Relaxed) + size;
    PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
}

#[inline]
fn measure_sub(size: usize) {
    LIVE_BYTES.fetch_sub(size, Ordering::Relaxed);
}

pub fn set_external_allocator(allocator: ExternalAllocator) -> bool {
    let mut was_set = false;
    let extern_allocator = EXTERN_ALLOCATOR.get_or_init(|| {
        was_set = true;
        allocator
    });
    if was_set {
        was_set = false;
        SELECTED_ALLOCATOR.get_or_init(|| {
            was_set = true;
            extern_allocator
        });
        // Selected before any allocation, so every byte counted as held is also counted freed.
        if was_set {
            measure_from_environment();
        }
    }
    was_set
}

/// Sets [`MEASURE`] where `LORE_MEMORY_STATS` asks for it, when the global allocator is selected:
/// counting from the first allocation keeps what is freed within what was counted as held. Reads
/// the environment without allocating.
fn measure_from_environment() {
    unsafe {
        let measure = libc::getenv(c"LORE_MEMORY_STATS".as_ptr().cast());
        if !measure.is_null() && *measure != 0 && libc::strcmp(measure, c"0".as_ptr().cast()) != 0 {
            TRACKING_ALLOCATIONS.fetch_or(MEASURE, Ordering::Relaxed);
        }
    }
}

fn default_allocator() -> &'static (dyn GlobalAlloc + Sync) {
    measure_from_environment();
    unsafe {
        let allocator = libc::getenv(c"LORE_ALLOCATOR".as_ptr().cast());
        if !allocator.is_null() && !libc::strstr(allocator, c"tracking".as_ptr().cast()).is_null() {
            TRACKING_ALLOCATIONS.fetch_or(tracking::TRACK, Ordering::Relaxed);
        }
        if !allocator.is_null() && !libc::strstr(allocator, c"system".as_ptr().cast()).is_null() {
            &STANDARD_ALLOCATOR
        } else {
            &rpmalloc::RPMALLOC_ALLOCATOR
        }
    }
}

static RPMALLOC_HEAP_ALLOCATOR: OnceLock<RpmallocHeapAllocator> = OnceLock::new();
static GROWVEC_ALLOCATOR: OnceLock<&'static (dyn GlobalAlloc + Sync)> = OnceLock::new();

/// Returns the allocator used for `GrowVec` chunk allocations. Defaults to a
/// dedicated rpmalloc heap, falls back to System if `LORE_ALLOCATOR=system`,
/// or uses an external allocator if one was set.
pub fn growvec_allocator() -> &'static (dyn GlobalAlloc + Sync) {
    *GROWVEC_ALLOCATOR.get_or_init(|| {
        if let Some(allocator) = EXTERN_ALLOCATOR.get() {
            return allocator as &'static (dyn GlobalAlloc + Sync);
        }
        unsafe {
            let allocator = libc::getenv(c"LORE_ALLOCATOR".as_ptr().cast());
            if !allocator.is_null() && !libc::strstr(allocator, c"system".as_ptr().cast()).is_null()
            {
                &STANDARD_ALLOCATOR
            } else {
                RPMALLOC_HEAP_ALLOCATOR.get_or_init(RpmallocHeapAllocator::default)
            }
        }
    })
}

pub fn growvec_allocator_stats() -> RpmallocHeapStatistics {
    if let Some(allocator) = RPMALLOC_HEAP_ALLOCATOR.get() {
        let heap = allocator.heap.lock();
        unsafe { rpmalloc::rpmalloc_heap_statistics(*heap) }
    } else {
        RpmallocHeapStatistics::default()
    }
}

static NODE_BLOCK_HEAP_ALLOCATOR: OnceLock<RpmallocHeapAllocator> = OnceLock::new();
static NODE_BLOCK_ALLOCATOR: OnceLock<Option<&'static (dyn GlobalAlloc + Sync)>> = OnceLock::new();

/// Returns the dedicated heap for revision-tree block payloads — node blocks,
/// their name tables and file metadata blocks — or `None` when the global
/// allocator cannot free what this heap hands out.
///
/// Those three are the revision tree's bulk memory and are allocated and freed
/// in one size class range on every worker thread. Keeping them out of the
/// general heap stops them churning each thread's page cache, and makes the
/// tree's footprint separately measurable through
/// [`node_block_allocator_stats`].
///
/// `None` is not a failure: callers fall back to the global allocator, which is
/// what happens under `LORE_ALLOCATOR=system` or an external allocator, neither
/// of which can hand out a private heap.
///
/// Every allocation from this heap must be freed back through it, because an
/// rpmalloc first-class heap has no owning thread and so counts *all* threads as
/// its owner: a free that skips the heap's mutex takes rpmalloc's unsynchronized
/// thread-local path and corrupts the page's free list. That is why the payloads
/// live in [`HeapBox`] and [`HeapBuf`], which remember their allocator, rather
/// than in `Box` and `BytesMut`, which do not.
pub fn node_block_allocator() -> Option<&'static (dyn GlobalAlloc + Sync)> {
    *NODE_BLOCK_ALLOCATOR.get_or_init(|| {
        if EXTERN_ALLOCATOR.get().is_some() {
            return None;
        }
        unsafe {
            let allocator = libc::getenv(c"LORE_ALLOCATOR".as_ptr().cast());
            if !allocator.is_null() && !libc::strstr(allocator, c"system".as_ptr().cast()).is_null()
            {
                None
            } else {
                Some(
                    NODE_BLOCK_HEAP_ALLOCATOR.get_or_init(RpmallocHeapAllocator::default)
                        as &'static (dyn GlobalAlloc + Sync),
                )
            }
        }
    })
}

pub fn node_block_allocator_stats() -> RpmallocHeapStatistics {
    if let Some(allocator) = NODE_BLOCK_HEAP_ALLOCATOR.get() {
        let heap = allocator.heap.lock();
        unsafe { rpmalloc::rpmalloc_heap_statistics(*heap) }
    } else {
        RpmallocHeapStatistics::default()
    }
}

/// Whether `LORE_MEMORY_STATS` asks for memory reports: at each sync phase and when a command ends.
///
/// The allocator is rpmalloc, whose memory the operating system's malloc tools (`heap`, Instruments,
/// `MallocStackLogging`) do not see, so this is how a run reports what it used. For call stacks, run
/// with `LORE_ALLOCATOR=system` under those tools, or with `LORE_ALLOCATOR=tracking`.
pub fn memory_stats_requested() -> bool {
    static REQUESTED: OnceLock<bool> = OnceLock::new();
    *REQUESTED.get_or_init(|| {
        std::env::var_os("LORE_MEMORY_STATS").is_some_and(|value| !value.is_empty() && value != "0")
    })
}

/// One line describing what the allocator and its dedicated heaps hold now, and the global
/// allocator's peak, in MiB.
///
/// Under `LORE_ALLOCATOR=system` or an external allocator rpmalloc holds nothing and the figures are
/// zero; the process's resident size is what the operating system reports then.
pub fn memory_summary() -> String {
    const MIB: usize = 1024 * 1024;
    let global = rpmalloc_global_stats();
    let growvec = growvec::memory_stats();
    let node_blocks = node_block_allocator_stats();
    format!(
        "memory: held {} MiB (peak {}); rpmalloc active {} MiB (peak {}), mapped {} MiB (peak {}), committed {} MiB, huge allocations peak {} MiB, heaps {}; growvec used {} MiB of {} MiB allocated; node blocks {} MiB allocated",
        LIVE_BYTES.load(Ordering::Relaxed) / MIB,
        PEAK_BYTES.load(Ordering::Relaxed) / MIB,
        global.active / MIB,
        global.active_peak / MIB,
        global.mapped / MIB,
        global.mapped_peak / MIB,
        global.committed / MIB,
        global.huge_alloc_peak / MIB,
        global.heap_count,
        growvec.used_bytes / MIB,
        growvec.allocated_bytes / MIB,
        node_blocks.allocated_bytes / MIB,
    )
}

pub fn rpmalloc_global_stats() -> RpmallocGlobalStatistics {
    let mut stats = RpmallocGlobalStatistics::default();
    unsafe {
        rpmalloc::rpmalloc_global_statistics_ffi(&mut stats);
    }
    stats
}

struct LoreAllocator;

unsafe impl GlobalAlloc for LoreAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe {
            SELECTED_ALLOCATOR
                .get_or_init(|| default_allocator())
                .alloc(layout)
        };
        let tracking = TRACKING_ALLOCATIONS.load(Ordering::Relaxed);
        if tracking != 0 && !ptr.is_null() {
            if (tracking & MEASURE) != 0 {
                measure_add(layout.size());
            }
            if (tracking & !MEASURE) != 0 {
                tracking::track_alloc(tracking, ptr, layout.size());
            }
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let tracking = TRACKING_ALLOCATIONS.load(Ordering::Relaxed);
        if tracking != 0 && !ptr.is_null() {
            if (tracking & MEASURE) != 0 {
                measure_sub(layout.size());
            }
            if (tracking & tracking::TRACK) != 0 {
                tracking::track_dealloc(ptr);
            }
        }
        unsafe {
            SELECTED_ALLOCATOR
                .get_or_init(|| default_allocator())
                .dealloc(ptr, layout);
        }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe {
            SELECTED_ALLOCATOR
                .get_or_init(|| default_allocator())
                .alloc_zeroed(layout)
        };
        let tracking = TRACKING_ALLOCATIONS.load(Ordering::Relaxed);
        if tracking != 0 && !ptr.is_null() {
            if (tracking & MEASURE) != 0 {
                measure_add(layout.size());
            }
            if (tracking & !MEASURE) != 0 {
                tracking::track_alloc(tracking, ptr, layout.size());
            }
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe {
            SELECTED_ALLOCATOR
                .get_or_init(|| default_allocator())
                .realloc(ptr, layout, new_size)
        };
        let tracking = TRACKING_ALLOCATIONS.load(Ordering::Relaxed);
        if tracking != 0 {
            if (tracking & MEASURE) != 0 && !new_ptr.is_null() {
                measure_add(new_size);
                measure_sub(layout.size());
            }
            if (tracking & !MEASURE) != 0 {
                tracking::track_realloc(tracking, ptr, new_ptr, layout.size());
            }
        }
        new_ptr
    }
}

pub use tracking::set_allocation_counting;
pub use tracking::spawn_allocation_file_dump;
pub use tracking::thread_allocation_count;

#[global_allocator]
static GLOBAL: LoreAllocator = LoreAllocator;
