/* rpmalloc.c  -  Memory allocator  -  2016-2020 Mattias Jansson
 *
 * SPDX-FileCopyrightText: 2016-2020 Mattias Jansson
 * SPDX-License-Identifier: Unlicense OR MIT
 *
 * This library provides a cross-platform lock free thread caching malloc
 * implementation in C11. The latest source code is always available at
 *
 * https://github.com/mjansson/rpmalloc
 *
 * This library is put in the public domain; you can redistribute it and/or
 * modify it without any restrictions. Or, if you choose, you can use it under
 * the MIT license.
 *
 */

#include "rpmalloc.h"

#include <errno.h>
#include <string.h>

#include <stdio.h>
#include <stdint.h>
#include <stdatomic.h>

#if !defined(__has_builtin)
#define __has_builtin(b) 0
#endif

#if defined(__clang__)
#pragma clang diagnostic ignored "-Wunused-macros"
#pragma clang diagnostic ignored "-Wunused-function"
#if __has_warning("-Wreserved-identifier")
#pragma clang diagnostic ignored "-Wreserved-identifier"
#endif
#if __has_warning("-Wstatic-in-inline")
#pragma clang diagnostic ignored "-Wstatic-in-inline"
#endif
#if __has_warning("-Wunsafe-buffer-usage")
#pragma clang diagnostic ignored "-Wunsafe-buffer-usage"
#endif
#if __has_warning("-Wimplicit-void-ptr-cast")
#pragma clang diagnostic ignored "-Wimplicit-void-ptr-cast"
#endif
#if __has_warning("-Wallocator-wrappers")
#pragma clang diagnostic ignored "-Wallocator-wrappers"
#endif
#elif defined(__GNUC__)
#pragma GCC diagnostic ignored "-Wunused-macros"
#pragma GCC diagnostic ignored "-Wunused-function"
#endif

#if defined(_WIN32) || defined(__WIN32__) || defined(_WIN64)
#define PLATFORM_WINDOWS 1
#define PLATFORM_POSIX 0
#else
#define PLATFORM_WINDOWS 0
#define PLATFORM_POSIX 1
#endif

#if defined(_MSC_VER)
#define NOINLINE __declspec(noinline)
#define FORCEINLINE __forceinline
#else
#define NOINLINE __attribute__((noinline))
#define FORCEINLINE inline __attribute__((always_inline))
#endif

#if PLATFORM_WINDOWS
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>
#include <fibersapi.h>
#include <intsafe.h>
static DWORD fls_key = FLS_OUT_OF_INDEXES;
//! VirtualAlloc2 (Windows 10+), resolved dynamically; null on older systems
typedef PVOID(WINAPI* virtualalloc2_fn)(HANDLE, PVOID, SIZE_T, ULONG, ULONG, MEM_EXTENDED_PARAMETER*, ULONG);
static virtualalloc2_fn os_virtualalloc2;
//! GetActiveProcessorCount (Windows 7+), resolved dynamically
typedef DWORD(WINAPI* active_processor_count_fn)(WORD);
#endif
#if PLATFORM_POSIX
#include <sys/mman.h>
#include <sys/resource.h>
#include <sched.h>
#include <unistd.h>
#include <fcntl.h>
#include <pthread.h>
static pthread_key_t pthread_key;
static int pthread_key_created;
#ifdef __FreeBSD__
#include <sys/sysctl.h>
#define MAP_HUGETLB MAP_ALIGNED_SUPER
#ifndef PROT_MAX
#define PROT_MAX(f) 0
#endif
#else
#define PROT_MAX(f) 0
#endif
#ifdef __sun
extern int
madvise(caddr_t, size_t, int);
#endif
#ifndef MAP_UNINITIALIZED
#define MAP_UNINITIALIZED 0
#endif
#endif

#if defined(__linux__) || defined(__ANDROID__)
#include <sys/prctl.h>
#include <sys/syscall.h>
#if !defined(MREMAP_MAYMOVE)
// The C library declares mremap and its flags only with _GNU_SOURCE, the flags have the values of the kernel interface
#define MREMAP_MAYMOVE 1
#define MREMAP_FIXED 2
extern void*
mremap(void* old_address, size_t old_size, size_t new_size, int flags, ...);
#endif
#if !defined(PR_SET_VMA)
#define PR_SET_VMA 0x53564d41
#define PR_SET_VMA_ANON_NAME 0
#endif
#endif
#if defined(__APPLE__)
#include <TargetConditionals.h>
#if !TARGET_OS_IPHONE && !TARGET_OS_SIMULATOR
#include <mach/mach_vm.h>
#include <mach/vm_statistics.h>
#endif
#include <mach-o/dyld.h>
#include <pthread.h>
#endif
#if defined(__HAIKU__) || defined(__TINYC__)
#include <pthread.h>
#endif

#include <limits.h>
#if (INTPTR_MAX > INT32_MAX)
#define ARCH_64BIT 1
#define ARCH_32BIT 0
#else
#define ARCH_64BIT 0
#define ARCH_32BIT 1
#endif

#define pointer_offset(ptr, ofs) (void*)((char*)(ptr) + (ptrdiff_t)(ofs))
#define pointer_diff(first, second) (ptrdiff_t)((const char*)(first) - (const char*)(second))

////////////
///
/// Build time configurable limits
///
//////

#ifndef ENABLE_VALIDATE_ARGS
//! Enable validation of args to public entry points
#define ENABLE_VALIDATE_ARGS 0
#endif
#ifndef ENABLE_ASSERTS
//! Enable asserts
#define ENABLE_ASSERTS 0
#endif
#ifndef ENABLE_UNMAP
//! Enable unmapping memory pages
#define ENABLE_UNMAP 1
#endif
#ifndef ENABLE_DECOMMIT
//! Enable decommitting memory pages
#define ENABLE_DECOMMIT 1
#endif
#ifndef ENABLE_DYNAMIC_LINK
//! Enable building as dynamic library
#define ENABLE_DYNAMIC_LINK 0
#endif
#ifndef ENABLE_OVERRIDE
//! Enable standard library malloc/free/new/delete overrides. When enabled, rpmalloc becomes the
//  backing store for the entire process, including the C runtime's own allocations (for example
//  per-thread TLS allocated by the loader). Two finalize-time features are therefore adjusted:
//  the unmap_on_finalize config option is ignored (see rpmalloc_initialize), since returning all
//  mappings to the OS while the process keeps running would unmap memory the runtime still holds;
//  and ENABLE_LEAK_DETECTION defaults off, since the runtime's still-live allocations are
//  indistinguishable from application leaks at finalize.
#define ENABLE_OVERRIDE 1
#endif
#ifndef ENABLE_STATISTICS
//! Enable statistics
#define ENABLE_STATISTICS 0
#endif
#ifndef ENABLE_THP
//! Enable transparent huge pages unless the configuration passed to rpmalloc_initialize_config says otherwise, for
//  builds where the program cannot pass a configuration, as with the standard library override
#define ENABLE_THP 0
#endif
#ifndef ENABLE_LEAK_DETECTION
//! Enable detection of allocations still outstanding at rpmalloc_finalize. Requires ENABLE_STATISTICS
//  for the allocation counters, and follows it by default - except under ENABLE_OVERRIDE, where it
//  defaults off: the override makes rpmalloc the backing store for the C runtime's own allocations
//  (for example per-thread TLS allocated by the loader), which are still live at finalize and
//  indistinguishable from application leaks, so the check would always report false positives.
#if ENABLE_OVERRIDE
#define ENABLE_LEAK_DETECTION 0
#else
#define ENABLE_LEAK_DETECTION ENABLE_STATISTICS
#endif
#endif
#if ENABLE_LEAK_DETECTION && !ENABLE_STATISTICS
#error ENABLE_LEAK_DETECTION requires ENABLE_STATISTICS
#endif

////////////
///
/// Built in size configurations
///
//////

//! Stride of the page structures packed in the span metadata
#define PAGE_HEADER_SIZE 128
//! Size of the span header, which overlays the first entries of the span slot table
#define SPAN_HEADER_SIZE 128
//! Offset of the block in a huge block span, past the span header and the page structure of the block
#define HUGE_HEADER_SIZE (SPAN_HEADER_SIZE + PAGE_HEADER_SIZE)

//! Alignment of each heap control structure within a shared mapping. Keeps adjacent
//  heaps on separate cache lines so cross-thread writes to one heap (thread_free) do
//  not falsely share with a neighbour heap owned by another thread. Sized for a 128 byte
//  cache line (Apple Silicon, x86 destructive interference) to cover 64 byte lines too.
#define HEAP_ALIGNMENT 128

#define SMALL_GRANULARITY 16

#define SMALL_BLOCK_SIZE_LIMIT (4 * 1024)
#define MEDIUM_SMALL_BLOCK_SIZE_LIMIT (32 * 1024)
#define MEDIUM_LARGE_BLOCK_SIZE_LIMIT (256 * 1024)
#define LARGE_BLOCK_SIZE_LIMIT (2 * 1024 * 1024)

#define SMALL_SIZE_CLASS_COUNT 73
#define MEDIUM_SMALL_SIZE_CLASS_COUNT 12
#define MEDIUM_LARGE_SIZE_CLASS_COUNT 12
#define LARGE_SIZE_CLASS_COUNT 12
#define SIZE_CLASS_COUNT \
	(SMALL_SIZE_CLASS_COUNT + MEDIUM_SMALL_SIZE_CLASS_COUNT + MEDIUM_LARGE_SIZE_CLASS_COUNT + LARGE_SIZE_CLASS_COUNT)

#define SMALL_PAGE_SIZE_SHIFT 16
#define SMALL_PAGE_SIZE (1 << SMALL_PAGE_SIZE_SHIFT)
#define SMALL_PAGE_MASK (~((uintptr_t)SMALL_PAGE_SIZE - 1))
#define MEDIUM_SMALL_PAGE_SIZE_SHIFT 20
#define MEDIUM_SMALL_PAGE_SIZE (1 << MEDIUM_SMALL_PAGE_SIZE_SHIFT)
#define MEDIUM_SMALL_PAGE_MASK (~((uintptr_t)MEDIUM_SMALL_PAGE_SIZE - 1))
#define MEDIUM_LARGE_PAGE_SIZE_SHIFT 22
#define MEDIUM_LARGE_PAGE_SIZE (1 << MEDIUM_LARGE_PAGE_SIZE_SHIFT)
#define MEDIUM_LARGE_PAGE_MASK (~((uintptr_t)MEDIUM_LARGE_PAGE_SIZE - 1))
#define LARGE_PAGE_SIZE_SHIFT 24
#define LARGE_PAGE_SIZE (1 << LARGE_PAGE_SIZE_SHIFT)
#define LARGE_PAGE_MASK (~((uintptr_t)LARGE_PAGE_SIZE - 1))

#define SPAN_SIZE (256 * 1024 * 1024)
#define SPAN_MASK (~((uintptr_t)(SPAN_SIZE - 1)))

//! A span is divided in slots of the small page size. The slot table at the start of the span maps each slot to
//  the page holding it, so the page of a block is found with one load from an address computed from the block
//  address alone. A page larger than a slot is mapped by each of its slots
#define SPAN_SLOT_SHIFT SMALL_PAGE_SIZE_SHIFT
#define SPAN_SLOT_COUNT (SPAN_SIZE >> SPAN_SLOT_SHIFT)
//! log2 of the slot table entry size
#define SPAN_SLOT_ENTRY_SHIFT 4
#define SPAN_SLOT_TABLE_SIZE (SPAN_SLOT_COUNT << SPAN_SLOT_ENTRY_SHIFT)
//! Number of slot table entries at the start of a huge block span that map to the huge block page. A huge block
//  starts at most RPMALLOC_MAX_ALIGNMENT from the span start
#define SPAN_HEAD_SLOT_COUNT 4
//! The page structures of a span of pages are packed after the slot table, indexed by page
#define SPAN_PAGE_ARRAY_OFFSET SPAN_SLOT_TABLE_SIZE
//! The slot table and page structures of a span of small pages cover its first pages, which are not used
#define SPAN_SMALL_METADATA_SIZE (SPAN_PAGE_ARRAY_OFFSET + ((SPAN_SIZE / SMALL_PAGE_SIZE) * PAGE_HEADER_SIZE))
#define SPAN_SMALL_FIRST_PAGE ((SPAN_SMALL_METADATA_SIZE + (SMALL_PAGE_SIZE - 1)) / SMALL_PAGE_SIZE)
//! Spans of larger pages keep the metadata in the first page, which is not used
#define SPAN_FIRST_PAGE 1
//! Metadata of spans committing pages on demand is committed in chunks of this size, the slot table and the
//  first chunk of page structures when the span is mapped
#define SPAN_METADATA_COMMIT_CHUNK (64 * 1024)
#define SPAN_METADATA_COMMIT_INITIAL (SPAN_PAGE_ARRAY_OFFSET + SPAN_METADATA_COMMIT_CHUNK)

#if ENABLE_VALIDATE_ARGS
//! Maximum allocation size to avoid integer overflow
#undef  MAX_ALLOC_SIZE
#define MAX_ALLOC_SIZE (((size_t)-1) - SPAN_SIZE)
#endif

////////////
///
/// Utility macros
///
//////

#if ENABLE_ASSERTS
#undef NDEBUG
#if defined(_MSC_VER) && !defined(_DEBUG)
#define _DEBUG
#endif
#include <assert.h>
#define RPMALLOC_TOSTRING_M(x) #x
#define RPMALLOC_TOSTRING(x) RPMALLOC_TOSTRING_M(x)
#define rpmalloc_assert(truth, message) \
	do {                                \
		if (!(truth)) {                 \
			assert((truth) && message); \
		}                               \
	} while (0)
#else
#define rpmalloc_assert(truth, message) \
	do {                                \
	} while (0)
#endif

#if defined(_MSC_VER)
#define rpmalloc_assume(cond) __assume(cond)
#elif defined(__clang__) && __has_builtin(__builtin_assume)
#define rpmalloc_assume(cond) __builtin_assume(cond)
#elif defined(__GNUC__)
#define rpmalloc_assume(cond)           \
	do {                                \
		if (!__builtin_expect(cond, 0)) \
			__builtin_unreachable();    \
	} while (0)
#else
#define rpmalloc_assume(cond) 0
#endif

//! Compute num * size into total, returning non-zero if the multiplication overflows
static inline int
size_mul_overflow(size_t num, size_t size, size_t* total) {
#if PLATFORM_WINDOWS
	return (SizeTMult(num, size, total) != S_OK);
#else
	return __builtin_umull_overflow(num, size, total);
#endif
}

////////////
///
/// Statistics
///
//////

#if ENABLE_STATISTICS

typedef struct rpmalloc_statistics_t {
	atomic_size_t page_mapped;
	atomic_size_t page_mapped_peak;
	atomic_size_t page_commit;
	atomic_size_t page_decommit;
	atomic_size_t page_active;
	atomic_size_t page_active_peak;
	atomic_size_t huge_alloc;
	atomic_size_t huge_alloc_peak;
	atomic_size_t heap_count;
} rpmalloc_statistics_t;

static rpmalloc_statistics_t global_statistics;

//! Subtract from a statistics counter, clamping at zero. Counters are in units of the
//! configured page size and heaps with committed pages are retained across finalize and
//! initialize cycles where the page size can change, so strict accounting is not possible.
static void
statistics_sub_saturating(atomic_size_t* counter, size_t value) {
	size_t current = atomic_load_explicit(counter, memory_order_relaxed);
	size_t next;
	do {
		next = (current > value) ? (current - value) : 0;
	} while (
	    !atomic_compare_exchange_weak_explicit(counter, &current, next, memory_order_relaxed, memory_order_relaxed));
}

//! Per size class allocation counters, updated only by the owning thread (cross-thread frees are
//  deferred and accounted when the owner processes them), so plain integers suffice
typedef struct heap_size_use_t {
	int32_t alloc_current;
	int32_t alloc_peak;
	int32_t alloc_total;
	int32_t free_total;
} heap_size_use_t;

//! A successful allocation of the given size class on the owning heap
#define _rpmalloc_stat_inc_alloc(heap, class_idx)                       \
	do {                                                                \
		heap_size_use_t* _su = (heap)->size_use + (class_idx);          \
		if (++_su->alloc_current > _su->alloc_peak)                     \
			_su->alloc_peak = _su->alloc_current;                       \
		++_su->alloc_total;                                             \
	} while (0)
//! Account for a number of freed blocks of the given size class on the owning heap
#define _rpmalloc_stat_add_free(heap, class_idx, count)                 \
	do {                                                                \
		heap_size_use_t* _su = (heap)->size_use + (class_idx);          \
		_su->alloc_current -= (int32_t)(count);                         \
		_su->free_total += (int32_t)(count);                            \
	} while (0)
//! A raw memory map call for a span of the given page type on the owning heap
#define _rpmalloc_stat_inc_span_map(heap, page_type) \
	do {                                             \
		++(heap)->span_map_calls[page_type];         \
	} while (0)
//! Move the used blocks of a page changing heaps to or from the counters of a heap, so the counters of every heap
//  stay at the blocks live in its pages when the heap taking the page counts their frees
#define _rpmalloc_stat_add_used(heap, class_idx, count)        \
	do {                                                       \
		heap_size_use_t* _su = (heap)->size_use + (class_idx); \
		_su->alloc_current += (int32_t)(count);                \
		if (_su->alloc_current > _su->alloc_peak)              \
			_su->alloc_peak = _su->alloc_current;              \
	} while (0)

#else

#define _rpmalloc_stat_inc_alloc(heap, class_idx) \
	do {                                          \
	} while (0)
#define _rpmalloc_stat_add_free(heap, class_idx, count) \
	do {                                                \
	} while (0)
#define _rpmalloc_stat_inc_span_map(heap, page_type) \
	do {                                             \
	} while (0)
#define _rpmalloc_stat_add_used(heap, class_idx, count) \
	do {                                                \
	} while (0)

#endif

////////////
///
/// Low level abstractions
///
//////

static inline size_t
rpmalloc_clz(uintptr_t x) {
#if ARCH_64BIT
#if defined(_MSC_VER) && !defined(__clang__)
#if defined(_M_ARM64)
	return (size_t)_CountLeadingZeros64(x);  // __lzcnt64 is x86-only
#else
	return (size_t)__lzcnt64(x);
#endif
#else
	return (size_t)__builtin_clzll(x);
#endif
#else
#if defined(_MSC_VER) && !defined(__clang__)
#if defined(_M_ARM64) || defined(_M_ARM)
	return (size_t)_CountLeadingZeros((unsigned long)x);  // __lzcnt32 is x86-only
#else
	return (size_t)__lzcnt32(x);
#endif
#else
	return (size_t)__builtin_clzl(x);
#endif
#endif
}

//! Yield the rest of this thread's scheduling quantum so another runnable thread (e.g. a lock
//! holder that has been preempted) can run. This is a real scheduler yield, unlike the per-arch
//! CPU pause/yield hint used by wait_spin below.
static inline void
thread_yield(void) {
#if PLATFORM_WINDOWS
	SwitchToThread();
#else
	sched_yield();
#endif
}

#ifndef RPMALLOC_TEST_YIELD_POINTS
#define RPMALLOC_TEST_YIELD_POINTS 0
#endif
#if RPMALLOC_TEST_YIELD_POINTS
//! Test builds yield the thread at every RPMALLOC_TEST_YIELD_POINTS-th pass of a point where another thread can
//  interleave, so the races the tests exercise happen often instead of in a window of a few instructions
static atomic_uint test_yield_counter;
#define test_yield_point()                                                                                           \
	do {                                                                                                             \
		if (!(atomic_fetch_add_explicit(&test_yield_counter, 1, memory_order_relaxed) % RPMALLOC_TEST_YIELD_POINTS)) \
			thread_yield();                                                                                          \
	} while (0)
#else
#define test_yield_point() \
	do {                   \
	} while (0)
#endif

#ifndef WAIT_SPIN_YIELD_THRESHOLD
//! Number of cheap CPU-pause spins before wait_spin escalates to a real OS scheduler yield.
#define WAIT_SPIN_YIELD_THRESHOLD 100
#endif

static inline void
wait_spin(uint32_t* spin_count) {
	if (++(*spin_count) >= WAIT_SPIN_YIELD_THRESHOLD) {
		thread_yield();
		return;
	}
#if defined(_MSC_VER)
#if defined(_M_ARM64)
	__yield();
#else
	_mm_pause();
#endif
#elif (defined(__x86_64__) || defined(__i386__)) && !defined(_M_ARM64EC)
	__asm__ volatile("pause" ::: "memory");
#elif defined(__aarch64__) || (defined(__arm__) && __ARM_ARCH >= 7) || defined(_M_ARM64EC)
	__asm__ volatile("yield" ::: "memory");
#elif defined(__powerpc__) || defined(__powerpc64__)
	// No idea if ever been compiled in such archs but ... as precaution
	__asm__ volatile("or 27,27,27");
#elif defined(__sparc__)
	__asm__ volatile("rd %ccr, %g0 \n\trd %ccr, %g0 \n\trd %ccr, %g0");
#else
	struct timespec ts = {0};
	nanosleep(&ts, 0);
#endif
}

#if RPMALLOC_TEST_YIELD_POINTS
#if !PLATFORM_WINDOWS
#include <time.h>
#endif
//! Milliseconds the next push of a queued page to the pending list of its heap waits before the push, set by the tests
//  to hold a thread in the window between queuing a page and pushing it while the owner works on the page
atomic_uint rpmalloc_test_pending_push_delay_ms;
static atomic_uint test_pending_push_count;

//! Milliseconds the next push of a block freed into a full page waits before it reads the heap of the page, set by
//  the tests to hold a thread that found the page full while other threads orphan the page and take it
atomic_uint rpmalloc_test_full_free_delay_ms;

static void
test_sleep_us(unsigned int delay_us) {
#if PLATFORM_WINDOWS
	Sleep((delay_us + 999) / 1000);
#else
	struct timespec ts = {(time_t)(delay_us / 1000000), (long)(delay_us % 1000000) * 1000};
	nanosleep(&ts, 0);
#endif
}

//! Hold the thread pushing a queued page, for the delay the tests set or 100us on every eighth push
static void
test_pending_push_delay(void) {
	unsigned int delay_us =
	    atomic_exchange_explicit(&rpmalloc_test_pending_push_delay_ms, 0, memory_order_relaxed) * 1000;
	if (!delay_us && !(atomic_fetch_add_explicit(&test_pending_push_count, 1, memory_order_relaxed) & 7))
		delay_us = 100;
	if (delay_us)
		test_sleep_us(delay_us);
}

//! Hold the thread pushing a block freed into a full page for the delay the tests set, or yield at a yield point
static void
test_full_free_delay(void) {
	unsigned int delay_ms = 0;
	if (atomic_load_explicit(&rpmalloc_test_full_free_delay_ms, memory_order_relaxed))
		delay_ms = atomic_exchange_explicit(&rpmalloc_test_full_free_delay_ms, 0, memory_order_relaxed);
	if (delay_ms)
		test_sleep_us(delay_ms * 1000);
	else
		test_yield_point();
}
#else
#define test_pending_push_delay() \
	do {                          \
	} while (0)
#define test_full_free_delay() \
	do {                       \
	} while (0)
#endif

#if defined(__GNUC__) || defined(__clang__)

#define EXPECTED(x) __builtin_expect((x), 1)
#define UNEXPECTED(x) __builtin_expect((x), 0)
//! Align the hot entry points to a cache line. Unrelated code changes otherwise move them across fetch block
//  boundaries, which changes the speed of tight allocation loops by up to 12%
#define ENTRY_ALIGNED __attribute__((aligned(64)))

#else

#define EXPECTED(x) x
#define UNEXPECTED(x) x
#define ENTRY_ALIGNED

#endif

#if defined(__GNUC__) || defined(__clang__)
#ifdef __has_builtin
#if __has_builtin(__builtin_memcpy_inline)
#define memcpy_const(x, y, s) __builtin_memcpy_inline(x, y, s)
#else
#define memcpy_const(x, y, s)                                                                                   \
	do {                                                                                                        \
		_Static_assert(__builtin_choose_expr(__builtin_constant_p(s), 1, 0), "len must be a constant integer"); \
		memcpy(x, y, s);                                                                                        \
	} while (0)
#endif

#if __has_builtin(__builtin_memset_inline)
#define memset_const(x, y, s) __builtin_memset_inline(x, y, s)
#else
#define memset_const(x, y, s)                                                                                   \
	do {                                                                                                        \
		_Static_assert(__builtin_choose_expr(__builtin_constant_p(s), 1, 0), "len must be a constant integer"); \
		memset(x, y, s);                                                                                        \
	} while (0)
#endif
#endif
#endif

#ifndef memcpy_const
#define memcpy_const(x, y, s) memcpy(x, y, s)
#define memset_const(x, y, s) memset(x, y, s)
#endif

////////////
///
/// Data types
///
//////

//! A memory heap, per thread
typedef struct heap_t heap_t;
//! Span of memory pages
typedef struct span_t span_t;
//! Memory page
typedef struct page_t page_t;
//! Memory block
typedef struct block_t block_t;
//! Size class for a memory block
typedef struct size_class_t size_class_t;

//! Memory page type
typedef enum page_type_t {
	PAGE_SMALL,         // 64KiB
	PAGE_MEDIUM_SMALL,  // 1MiB
	PAGE_MEDIUM_LARGE,  // 4MiB
	PAGE_LARGE,         // 16MiB
	PAGE_HUGE
} page_type_t;

//! Block size class
struct size_class_t {
	//! Size of blocks in this class
	uint32_t block_size;
	//! Number of blocks in each chunk
	uint32_t block_count;
};

//! A memory block
struct block_t {
	//! Next block in list
	block_t* next;
};

//! A page contains blocks of a given size. The page structures are packed in the metadata of the span holding the
//  pages, not stored in the pages, so the structures of different pages do not map to the same cache sets and the
//  blocks start at the page start
struct page_t {
	// The first cache line holds the fields the owning heap writes on every local free and page refill, the
	// second line the fields other threads freeing into the page read and the thread free list they write, so
	// cross-thread frees and the owner do not take the line from each other on every free
	union {
		struct {
			//! Block used count
			uint32_t block_used;
			//! Block initialized count
			uint32_t block_initialized;
			//! Local free list
			block_t* local_free;
			//! Next page in list
			page_t* next;
			//! Previous page in list
			page_t* prev;
			//! Block count
			uint32_t block_count;
			//! Bytes of page memory committed from the page start. A page of a span committing on demand
			//  commits its memory in parts as its blocks are initialized, the other pages are committed whole
			uint32_t commit_size;
			//! Flag set if part of heap full list
			uint32_t is_full : 1;
			//! Flag set if part of heap free list
			uint32_t is_free : 1;
			//! Flag set if blocks are zero initialied
			uint32_t is_zero : 1;
			//! Flag set if memory pages have been decommitted
			uint32_t is_decommitted : 1;
			//! Set when a refill took blocks freed by the owner thread from the page local free list
			uint32_t local_reuse : 1;
			//! Set when the page received blocks freed by other threads
			uint32_t remote_fed : 1;
			//! Flag set if the page is the header of a huge block span in the huge span list of its heap
			uint32_t is_tracked : 1;
		};
		//! Padding of the owner fields to a cache line
		char owner_line[64];
	};
	//! Multithreaded free list, block offset is in low 32 bit, list count is high 32 bit
	atomic_ullong thread_free;
	//! Start of the page memory, where the first block starts. Kept next to the thread free list, which is coded
	//  with block offsets from it
	block_t* block_start;
	//! Owning heap
	heap_t* heap;
	//! Block size
	uint32_t block_size;
	//! Size class of blocks
	uint16_t size_class;
	//! Page type, kept out of the flag word so reading it never waits on the byte stores of flag updates
	uint16_t page_type;
	//! Flag set if containing aligned blocks.
	//  Note: has_aligned_block is read on the cross-thread free path while the owner thread writes
	//  this packed flag word. The owner is the sole writer and the word is naturally aligned
	//  (hardware-atomic access), and the read flag is monotonic, so the read/write race is benign.
	//  ThreadSanitizer still reports it; see test/tsan-suppressions.txt.
	uint32_t has_aligned_block : 1;
	//! Set if the span maps reserve-only (pages commit on demand, may be decommitted),
	//  captured at map time so it survives a config change across a finalize/initialize
	uint32_t commit_on_demand : 1;
	//! log2 of the commit granularity (the page size at the span's map time)
	uint32_t commit_shift : 6;
	//! Set if the heap was a first class heap when the page was initialized for its size class. Cross-thread
	//  frees into the page are not chained, read from the flag word the free path reads instead of the remotely
	//  written heap line. The heap flag is only ever cleared, so a stale page flag only stops chaining
	uint32_t heap_first_class : 1;
	//! Next page in the pending page list of the heap, written by the thread queueing the page
	page_t* pending_next;
};

//! An entry of the span slot table
typedef struct page_slot_t {
	//! Page holding the slot. First in the entry, the page feeds the free list updates of a free while the key
	//  only feeds a predicted branch, so its load gets the cheaper address computation
	page_t* page;
	//! Fast free key of the page holding the slot, the owning heap with the low bit set while the page takes the
	//  fast free path (not full, not huge and holding no aligned blocks), and zero otherwise. Heaps are aligned,
	//  so a key never equals the low bit set on another heap or on the null heap of a thread without one, and the
	//  free fast path checks ownership and eligibility with a single compare. Threads freeing into the page read
	//  it while the owner updates it, and never find their own heap in it
	atomic_uintptr_t key;
} page_slot_t;

//! Fast free key of a page that takes the generic free path
#define PAGE_KEY_GENERIC ((uintptr_t)0)

//! Owner thread of the orphan heap holding the orphan pages, which no thread owns
#define ORPHAN_HEAP_OWNER ((uintptr_t)1)

//! A span contains pages of a given type, or a single huge block. The span starts with the slot table, followed
//  by the page structures, both committed on demand. The span header overlays the first slot table entries, which
//  in a span of pages map slots holding metadata and are never looked up. A huge block span holds the span header,
//  the page structure of the huge block and the block, and the overlaid slot table entries map the huge block page
struct span_t {
	//! Slot table entries overlaid by the span header, only set in a huge block span
	page_slot_t slot_head[SPAN_HEAD_SLOT_COUNT];
	//! Owning heap
	heap_t* heap;
	//! Mapped size
	uint64_t mapped_size;
	//! Next span in list
	span_t* next;
	//! Number of pages initialized, including the pages holding metadata
	uint32_t page_initialized;
	//! Number of pages in the span. For a huge block span the committed pages, which include the slack kept
	//  from a reused mapping
	uint32_t page_count;
	//! Number of bytes per page
	uint32_t page_size;
	//! Page type
	page_type_t page_type;
	//! Offset to start of mapped memory region
	uint32_t offset;
	//! Number of pages requested for a huge block, its usable size
	uint32_t page_request;
	//! Number of bytes of metadata committed, for spans committing on demand
	uint32_t metadata_commit;
	//! Set if the span maps reserve-only (pages commit on demand, may be decommitted). A huge block span never
	//  takes the per page commit path, the flag tells whether the huge block cache can commit and decommit its tail
	uint32_t commit_on_demand : 1;
	//! log2 of the commit granularity (the page size at map time)
	uint32_t commit_shift : 6;
};

// Control structure for a heap, either a thread heap or a first class heap if enabled
struct heap_t {
	// The first HEAP_ALIGNMENT bytes hold the fields other threads access: owner_thread is read and
	// thread_free is written by every cross-thread free into a full page. Keeping them apart from the
	// owner's hot allocation state avoids invalidating the owner's local free lists on every remote
	// free and the remote CAS line being pulled away by owner writes.
	//! Owning thread ID
	uintptr_t owner_thread;
	//! Multithreaded free list
	atomic_uintptr_t thread_free[4];
	//! Pages other threads freed every used block of, linked through the page pending next pointer
	atomic_uintptr_t page_pending;
	//! Padding to the end of the remotely accessed line
	char remote_pad[HEAP_ALIGNMENT - (sizeof(uintptr_t) + (5 * sizeof(atomic_uintptr_t)))];
	//! Heap local free list for small size classes
	block_t* local_free[SIZE_CLASS_COUNT];
	//! Available non-full pages for each size class
	page_t* page_available[SIZE_CLASS_COUNT];
	//! Free pages for each page type, committed pages first
	page_t* page_free[4];
	//! Last page in the free page list for each page type
	page_t* page_free_tail[4];
	//! Free but still committed page count for each page tyoe
	uint32_t page_free_commit_count[4];
	//! Time (monotonic ms) the decommit window of each page type started, zero while the free committed page count
	//  is at most the retain count
	uint64_t page_free_overflow_ms[4];
	//! Lowest free committed page count of each page type since its decommit window started
	uint32_t page_free_low[4];
	//! Last decommit epoch the heap freed pages in, counted in the heaps active in that epoch
	uint32_t page_free_epoch;
	//! Size decommitted from free pages of other page types beyond what committed pages needed, in small
	//  page units, used by the next commits before decommitting more
	uint32_t page_commit_credit;
	//! Available partially initialized spans for each page type
	span_t* span_partial[4];
	//! Spans in full use for each page type
	span_t* span_used[5];
	//! Next heap in queue
	heap_t* next;
	//! Previous heap in queue
	heap_t* prev;
	//! Heap ID
	uint32_t id;
	//! Finalization state flag
	uint32_t finalize;
	//! Set when the heap was released without being trimmed, the thread adopting it trims it first
	uint32_t release_trim;
	//! Set while the heap is in the queue of released heaps with its pages handed to the orphan lists, written
	//  under the heap lock
	atomic_uint orphaned;
	//! Set while the orphan pass hands the pages of the released heap to the orphan lists under the heap lock, the
	//  pages it frees are then not decommitted, the pass hands them on and decommits them after the lock
	uint32_t orphaning;
	//! Time (monotonic ms) the heap was released, while in the queue of released heaps
	uint64_t release_ms;
	//! Idle state of a thread heap, HEAP_STATE_ACTIVE while its thread uses it, HEAP_STATE_IDLE while its thread
	//  waits for work, and HEAP_STATE_REAPING while another thread reaps it. The idle fields are ordered to take 32
	//  bytes without padding
	atomic_uint idle_state;
	//! Count of the refills and page retirements of the heap, which a reap compares with the count at the last
	//  reap to tell a heap used since
	uint32_t activity;
	//! Activity count at the last reap of the heap
	uint32_t reaped_activity;
	//! Activity count when idle_ms was taken
	uint32_t idle_activity;
	//! Time (monotonic ms) of the first time the thread of the heap began waiting for work since the heap was last
	//  used, a thread woken without work to do keeps it
	uint64_t idle_ms;
	//! Time (monotonic ms) of the last reap of the heap, or of the last check finding nothing to reap
	uint64_t reap_ms;
	//! Set for a first class heap, written before the heap has any page and cleared when a thread takes the
	//  released heap over. Copied to the pages initialized for a size class, other threads never hold blocks of
	//  such a page in their free chain, since rpmalloc_heap_free_all unmaps the pages regardless
	uint32_t first_class;
	//! Memory map region offset
	uint32_t offset;
	//! Memory map size
	size_t mapped_size;
#if RPMALLOC_HEAP_STATISTICS
	struct rpmalloc_heap_statistics_t stats;
#endif
#if ENABLE_STATISTICS
	//! Per size class allocation counters (current span_use count is computed live on demand)
	heap_size_use_t size_use[SIZE_CLASS_COUNT];
	//! Per page type raw span map call counters
	uint32_t span_map_calls[5];
#endif
};

_Static_assert(sizeof(page_t) <= PAGE_HEADER_SIZE, "Invalid page header size");
_Static_assert(sizeof(span_t) <= SPAN_HEADER_SIZE, "Invalid span header size");
_Static_assert(sizeof(page_slot_t) == (1 << SPAN_SLOT_ENTRY_SHIFT), "Invalid slot table entry size");
_Static_assert((SPAN_HEADER_SIZE >> SPAN_SLOT_ENTRY_SHIFT) <= SPAN_SMALL_FIRST_PAGE,
               "Span header overlays the slot table entries of used small pages");
_Static_assert((SPAN_HEADER_SIZE >> SPAN_SLOT_ENTRY_SHIFT) <=
                   ((SPAN_FIRST_PAGE * MEDIUM_SMALL_PAGE_SIZE) >> SPAN_SLOT_SHIFT),
               "Span header overlays the slot table entries of used pages");
_Static_assert(SPAN_SMALL_METADATA_SIZE <= (SPAN_SMALL_FIRST_PAGE * SMALL_PAGE_SIZE),
               "Small span metadata overlaps pages");
_Static_assert((SPAN_PAGE_ARRAY_OFFSET + ((SPAN_SIZE / MEDIUM_SMALL_PAGE_SIZE) * PAGE_HEADER_SIZE)) <=
                   (SPAN_FIRST_PAGE * MEDIUM_SMALL_PAGE_SIZE),
               "Span metadata overlaps pages");
// An aligned huge block starts at the first multiple of the alignment past the header, below the maximum alignment
_Static_assert((RPMALLOC_MAX_ALIGNMENT <= (SPAN_HEAD_SLOT_COUNT << SPAN_SLOT_SHIFT)),
               "Huge block span head slots do not cover the largest alignment");
_Static_assert(HUGE_HEADER_SIZE <= (SPAN_HEAD_SLOT_COUNT << SPAN_SLOT_SHIFT), "Huge block span header too large");
_Static_assert(((SMALL_PAGE_SIZE >> SPAN_SLOT_SHIFT) == 1) &&
                   ((MEDIUM_SMALL_PAGE_SIZE >> SPAN_SLOT_SHIFT) == (1 << (2 + (2 * PAGE_MEDIUM_SMALL)))) &&
                   ((MEDIUM_LARGE_PAGE_SIZE >> SPAN_SLOT_SHIFT) == (1 << (2 + (2 * PAGE_MEDIUM_LARGE)))) &&
                   ((LARGE_PAGE_SIZE >> SPAN_SLOT_SHIFT) == (1 << (2 + (2 * PAGE_LARGE)))),
               "Slot count per page does not match page_set_key");
_Static_assert(sizeof(heap_t) <= 4096, "Invalid heap size");
_Static_assert(offsetof(heap_t, local_free) == HEAP_ALIGNMENT, "Remotely accessed heap fields must fill one line");

////////////
///
/// Global data
///
//////

//! Fallback heap
static RPMALLOC_CACHE_ALIGNED heap_t global_heap_fallback;
//! Default heap
static heap_t* global_heap_default = &global_heap_fallback;
//! Released heaps available for reuse. These have been used and may hold caches, so they are
//  only handed to ordinary thread heaps, never to first-class heaps (which require pristine state)
static heap_t* global_heap_queue;
//! Heap of the queue of released heaps the next orphan pass starts at, the queue below it is not yet orphaned
static heap_t* global_heap_orphan_cursor;
//! Pristine, never-used heaps carved from a shared block but not yet handed out. Safe for any
//  request including first-class, which needs a heap with no prior allocations
static heap_t* global_heap_pristine;
//! In use heaps
static heap_t* global_heap_used;
//! Set while a thread is mapping and carving a new shared heap block, so concurrent thread
//  starts wait for the carved heaps to reach a pool instead of each mapping their own block.
//  A user memory interface callback must not allocate through rpmalloc on the creating thread:
//  a nested heap creation would wait on this flag set by the same thread and deadlock
static int global_heap_creating;
//! Lock for heap queue
static atomic_uintptr_t global_heap_lock;
//! Heap ID counter
static atomic_uint global_heap_id = 1;
//! Global initialization state. rpmalloc_initialize can be called concurrently from multiple
//! threads (the first allocation on any thread may trigger it), so it is driven as an atomic
//! state machine: exactly one thread performs the global setup while the others wait for it to
//! complete before allocating - they must never observe, or allocate against, a partially
//! written global_config. Release/acquire ordering on the DONE transition publishes all of the
//! global_config writes to threads that observe completion.
#define RPMALLOC_INIT_UNINIT 0
#define RPMALLOC_INIT_RUNNING 1
#define RPMALLOC_INIT_DONE 2
static atomic_int global_rpmalloc_init_state;
//! Memory interface
static rpmalloc_interface_t* global_memory_interface;
//! Default memory interface
static rpmalloc_interface_t global_memory_interface_default;
//! Current configuration
static rpmalloc_config_t global_config = {.enable_thp = ENABLE_THP};
//! Main thread ID
static uintptr_t global_main_thread_id;
//! Set when the module is unloaded or the process exits, the thread destructor is not registered again
static int global_module_detached;

//! Size classes
#define SCLASS(n) \
	{ (n * SMALL_GRANULARITY), SMALL_PAGE_SIZE / (n * SMALL_GRANULARITY) }
#define MSCLASS(n) \
	{ (n * SMALL_GRANULARITY), MEDIUM_SMALL_PAGE_SIZE / (n * SMALL_GRANULARITY) }
#define MLCLASS(n) \
	{ (n * SMALL_GRANULARITY), MEDIUM_LARGE_PAGE_SIZE / (n * SMALL_GRANULARITY) }
#define LCLASS(n) \
	{ (n * SMALL_GRANULARITY), LARGE_PAGE_SIZE / (n * SMALL_GRANULARITY) }
static const size_class_t global_size_class[SIZE_CLASS_COUNT] = {
    SCLASS(1),      SCLASS(1),      SCLASS(2),      SCLASS(3),      SCLASS(4),      SCLASS(5),      SCLASS(6),
    SCLASS(7),      SCLASS(8),      SCLASS(9),      SCLASS(10),     SCLASS(11),     SCLASS(12),     SCLASS(13),
    SCLASS(14),     SCLASS(15),     SCLASS(16),     SCLASS(17),     SCLASS(18),     SCLASS(19),     SCLASS(20),
    SCLASS(21),     SCLASS(22),     SCLASS(23),     SCLASS(24),     SCLASS(25),     SCLASS(26),     SCLASS(27),
    SCLASS(28),     SCLASS(29),     SCLASS(30),     SCLASS(31),     SCLASS(32),     SCLASS(33),     SCLASS(34),
    SCLASS(35),     SCLASS(36),     SCLASS(37),     SCLASS(38),     SCLASS(39),     SCLASS(40),     SCLASS(41),
    SCLASS(42),     SCLASS(43),     SCLASS(44),     SCLASS(45),     SCLASS(46),     SCLASS(47),     SCLASS(48),
    SCLASS(49),     SCLASS(50),     SCLASS(51),     SCLASS(52),     SCLASS(53),     SCLASS(54),     SCLASS(55),
    SCLASS(56),     SCLASS(57),     SCLASS(58),     SCLASS(59),     SCLASS(60),     SCLASS(61),     SCLASS(62),
    SCLASS(63),     SCLASS(64),     SCLASS(80),     SCLASS(96),     SCLASS(112),    SCLASS(128),    SCLASS(160),
    SCLASS(192),    SCLASS(224),    SCLASS(256),    MSCLASS(320),   MSCLASS(384),   MSCLASS(448),   MSCLASS(512),
    MSCLASS(640),   MSCLASS(768),   MSCLASS(896),   MSCLASS(1024),  MSCLASS(1280),  MSCLASS(1536),  MSCLASS(1792),
    MSCLASS(2048),  MLCLASS(2560),  MLCLASS(3072),  MLCLASS(3584),  MLCLASS(4096),  MLCLASS(5120),  MLCLASS(6144),
    MLCLASS(7168),  MLCLASS(8192),  MLCLASS(10240), MLCLASS(12288), MLCLASS(14336), MLCLASS(16384), LCLASS(20480),
    LCLASS(24576),  LCLASS(28672),  LCLASS(32768),  LCLASS(40960),  LCLASS(49152),  LCLASS(57344),  LCLASS(65536),
    LCLASS(81920),  LCLASS(98304),  LCLASS(114688), LCLASS(131072)};

#ifndef PAGE_FULL_RETAIN
//! Number of pages without free blocks a refill moves to the back of the available list before
//  marking pages full
#define PAGE_FULL_RETAIN 2
#endif

#ifndef PAGE_DECOMMIT_DELAY_MS
//! Length in milliseconds of the decommit window of free pages. The free committed pages of a page type a
//  heap holds through a whole window without reusing them are decommitted beyond the retain count.
//  Delaying the decommit keeps memory committed across allocate/free phases that would otherwise
//  decommit and page fault the same pages every cycle.
#define PAGE_DECOMMIT_DELAY_MS 250
#endif
#ifndef PAGE_DECOMMIT_LIMIT_FACTOR
//! Multiple of the overflow threshold at which free pages are decommitted immediately regardless of
//  the delay, bounding the committed memory a heap can hold in free pages
#define PAGE_DECOMMIT_LIMIT_FACTOR 4
#endif

//! Free page count of each page type at which taking a page of another type decommits free pages of the type,
//  and which times PAGE_DECOMMIT_LIMIT_FACTOR bounds the free committed pages within a decommit window
static uint32_t global_page_free_overflow[5] = {16, 8, 4, 2, 0};

//! Number of free committed pages of each page type a heap keeps for reuse past a decommit window
static uint32_t global_page_free_retain[5] = {4, 2, 1, 1, 0};

//! Size of each page type in small page units
static const uint32_t global_page_units[4] = {1, MEDIUM_SMALL_PAGE_SIZE / SMALL_PAGE_SIZE,
                                              MEDIUM_LARGE_PAGE_SIZE / SMALL_PAGE_SIZE,
                                              LARGE_PAGE_SIZE / SMALL_PAGE_SIZE};

//! Transparent huge page use, a bit per page type of span mappings and a bit for huge block mappings, for the
//  mappings advised to be backed by huge pages when mapped. Huge block mappings are, and small page spans once their
//  heap uses more than their first huge page. Spans of the other page types hold the partially used pages of the
//  larger size classes, where huge pages would make the whole huge page of each partially used page resident
static uint32_t global_thp_mask;

//! Page types of span mappings advised not to be backed by huge pages when mapped, when the system backs every
//  mapping by huge pages unless advised otherwise. Small page spans are advised to be backed by huge pages again
//  once their heap uses more than their first huge page
static uint32_t global_thp_nohuge_mask;

//! Index of the first page of a small page span past its first huge page, the span is advised to be backed by huge
//  pages when the heap initializes it. A heap that uses less than a huge page of small pages keeps them in small
//  pages instead of a whole huge page. Zero without transparent huge pages, below the first page of any span
static uint32_t global_thp_small_page;

static void*
os_mmap(size_t size, size_t alignment, size_t* offset, size_t* mapped_size);

//! Advise a fresh mapping of the OS interface to be backed by huge pages, before anything accesses it. Mappings of
//  a custom memory interface are left as the interface made them. The mapping is then written first: the first access
//  to the header is otherwise a read, whose fault maps the huge zero page, and the write fault after it splits the
//  zero page into small pages instead of allocating a huge page, leaving the header and the first pages of the
//  mapping in small pages
static void
thp_advise(void* address, size_t size, uint32_t type) {
#if defined(MADV_HUGEPAGE)
	if (global_memory_interface->memory_map != os_mmap)
		return;
	if (global_thp_nohuge_mask & (1U << type)) {
		(void)madvise(address, size, MADV_NOHUGEPAGE);
	} else if ((global_thp_mask & (1U << type)) && !madvise(address, size, MADV_HUGEPAGE)) {
		*(volatile uintptr_t*)address = 0;
		__asm__ __volatile__("" ::: "memory");
	}
#else
	(void)sizeof(address);
	(void)sizeof(size);
	(void)sizeof(type);
#endif
}

//! Advise a small page span to be backed by huge pages once its heap initializes the first page past its first huge
//  page. The pages before it are already in use and stay in small pages
static NOINLINE void
thp_advise_small_span(span_t* span) {
#if defined(MADV_HUGEPAGE)
	if (global_memory_interface->memory_map == os_mmap)
		(void)madvise(span, SPAN_SIZE, MADV_HUGEPAGE);
#else
	(void)sizeof(span);
#endif
}

//! OS huge page support
static int os_huge_pages;
//! OS transparent huge page support (advise mappings to be huge page backed without
//! requiring a preallocated huge page pool, currently Linux/Android only)
static int os_thp;
//! Size of a transparent huge page
static size_t os_thp_size;
//! Shift for the huge page size (log2 of huge page size when huge pages are enabled)
static size_t os_huge_page_shift;
//! OS memory map granularity
static size_t os_map_granularity;
//! OS memory page size
static size_t os_page_size;
//! Huge block mappings reserve the address range up to the next multiple of the span size, which the span alignment
//  leaves unusable for other spans and huge blocks, and commit only the block. A cached mapping is then reused for any
//  request up to its range and a block grows in place up to it. Only where the reserved range is not charged as
//  committed memory, which strict overcommit accounting and an address space limit both do
static int os_huge_reserve;

//! Size of the mapping of a huge block of the given size, the address range a mapping reserves. With transparent huge
//  pages only a block ending on a huge page boundary reserves beyond it, the fault of the last partial huge page of a
//  block would otherwise fill the whole huge page within the reserved range
static size_t
os_huge_reserve_size(size_t alloc_size) {
	if (!os_huge_reserve || ((global_thp_mask & (1U << PAGE_HUGE)) && (alloc_size & (os_thp_size - 1))))
		return alloc_size;
	return (alloc_size + (SPAN_SIZE - 1)) & ~(size_t)(SPAN_SIZE - 1);
}

////////////
///
/// Thread local heap and ID
///
//////

//! Current thread heap
#if defined(_MSC_VER) && !defined(__clang__)
#define _Thread_local __declspec(thread)
#endif
#ifndef TLS_MODEL
#if defined(_MSC_VER) && !defined(__clang__)
#define TLS_MODEL
#elif defined(__ANDROID__)
#if __ANDROID_API__ >= 29 && \
    ((defined(__clang__) && (__clang_major__ >= 17)) || (defined(__NDK_MAJOR__) && (__NDK_MAJOR__ >= 26)))
#define TLS_MODEL __attribute__((tls_model("local-dynamic")))
#else
#define TLS_MODEL
#endif
#elif defined(__linux__) && !defined(__GLIBC__) && defined(__PIC__) && !defined(__PIE__)
//! Shared object without glibc, musl rejects initial-exec TLS in a library loaded with dlopen
#define TLS_MODEL __attribute__((tls_model("local-dynamic")))
#else
#define TLS_MODEL __attribute__((tls_model("initial-exec")))
#endif
#endif
//! Per thread state. Besides the thread heap it holds a chain of blocks the thread freed into a page
//  owned by another heap and has not pushed to the page yet. A cross-thread free into the same page as
//  the previous one is linked into the chain without atomic operations, and the chain is pushed to the
//  page with a single compare and swap when the thread frees into another page, when the chain holds
//  THREAD_FREE_CHAIN_LIMIT_BYTES of blocks or at a flush point (its allocation slow path,
//  rpmalloc_thread_collect, rpmalloc_thread_collect_unused or rpmalloc_thread_finalize, which also runs from the
//  thread exit destructor).
//  Chained blocks are still counted as used by the page, so the page cannot be retired while the chain holds
//  them. The heap pointer and the chain share one thread local object so the cross-thread free path makes a
//  single thread local access
typedef struct thread_context_t {
	//! Current thread heap
	heap_t* heap;
	//! Page of the previous cross-thread free, the page the chain blocks belong to. A chain begins with the
	//  second free in a row into the page, which checks that the page can be batched, and the chained blocks
	//  keep the page from being retired. The page is kept after the chain is pushed, and cleared after a free
	//  into a page that cannot be batched
	page_t* chain_page;
	//! First block of the chain
	block_t* chain_head;
	//! Next pointer the following chained block is stored in, chain_head while the chain is empty
	block_t** chain_link;
	//! Number of blocks in the chain
	uint32_t chain_count;
	//! Thread exit push of the chain, see thread_free_chain_register
	uint32_t chain_exit;
	//! Heap the thread left idle in rpmalloc_thread_idle_begin, taken back by the thread before it uses the
	//  allocator again. The thread heap is the fallback heap meanwhile, so any allocation takes the path that
	//  initializes a thread, which takes this heap back
	heap_t* idle_heap;
} thread_context_t;

//! Bytes of blocks a thread free chain holds before it is pushed, bounding the memory a thread that frees into a
//  page of another heap and then stops at no flush point (a consumer thread blocking on a wait) keeps from the
//  owner. The default is above the small page size, so a chain of small blocks is pushed whole when the thread
//  leaves the page, handing the owner the page as one run, while a chain of medium small blocks (1MiB pages) is
//  pushed in parts. Zero pushes the chain only when the thread frees into another page or reaches a flush point
#ifndef THREAD_FREE_CHAIN_LIMIT_BYTES
#define THREAD_FREE_CHAIN_LIMIT_BYTES (256 * 1024)
#endif

//! No thread exit push of the chain is registered
#define CHAIN_EXIT_UNREGISTERED 0
//! The chain is pushed when the thread exits
#define CHAIN_EXIT_REGISTERED 1
//! The thread has finalized (or exited, on macOS) and no longer batches unless it has a heap, since the
//  thread exit destructor may not run another time
#define CHAIN_EXIT_FINALIZED 2

static _Thread_local thread_context_t global_thread TLS_MODEL = {&global_heap_fallback, 0, 0, 0, 0, 0, 0};

static heap_t*
heap_allocate(int first_class);

static void
heap_release_free_pages(heap_t* heap);

static NOINLINE void
heap_collect_pending_pages(heap_t* heap);

static NOINLINE void
heap_collect_thread_free(heap_t* heap);

static void
heap_orphan_pages(heap_t* heap);

static page_t*
heap_orphan_free_pages(heap_t* heap, page_t* chain);

static void
page_orphan_empty_add(page_t* page);

static void
page_orphan_sweep(uint64_t now);

static inline int
heap_make_free_page_available(heap_t* heap, uint32_t size_class, page_t* page);

#ifndef HEAP_ORPHAN_DELAY_MS
//! Time in milliseconds a released heap stays in the queue of released heaps with its pages, for a thread adopting
//  it, before the pages are handed to the orphan lists
#define HEAP_ORPHAN_DELAY_MS 1000
#endif
#ifndef HEAP_ORPHAN_BATCH
//! Number of released heaps whose pages are handed to the orphan lists at most per heap release
#define HEAP_ORPHAN_BATCH 4
#endif
#if RPMALLOC_TEST_YIELD_POINTS
//! Orphan delay of test builds, the tests shorten it to orphan the heaps of exiting threads right away
atomic_uint rpmalloc_test_orphan_delay_ms = HEAP_ORPHAN_DELAY_MS;
#define heap_orphan_delay_ms() atomic_load_explicit(&rpmalloc_test_orphan_delay_ms, memory_order_relaxed)
#else
#define heap_orphan_delay_ms() HEAP_ORPHAN_DELAY_MS
#endif

static NOINLINE void
heap_page_free_overflow(heap_t* heap, uint32_t page_type);

static NOINLINE void
heap_page_free_overflow_other(heap_t* heap, uint32_t page_type);

//! Check if the decommit window of free pages of another page type than the given one is running
static inline int
heap_page_free_overflow_other_pending(heap_t* heap, uint32_t page_type) {
	uint64_t pending = 0;
	for (uint32_t itype = 0; itype < 4; ++itype)
		pending |= (itype != page_type) ? heap->page_free_overflow_ms[itype] : 0;
	return (pending != 0);
}

//! Coarse monotonic time in milliseconds (no syscall on Linux/vDSO)
static inline uint64_t
monotonic_time_ms(void) {
#if PLATFORM_WINDOWS
	return (uint64_t)GetTickCount64();
#else
	struct timespec ts;
#if defined(CLOCK_MONOTONIC_COARSE)
	clock_gettime(CLOCK_MONOTONIC_COARSE, &ts);
#else
	clock_gettime(CLOCK_MONOTONIC, &ts);
#endif
	return ((uint64_t)ts.tv_sec * 1000ULL) + ((uint64_t)ts.tv_nsec / 1000000ULL);
#endif
}

//! Fast thread ID
static inline uintptr_t
get_thread_id(void) {
#if defined(_WIN32)
	return (uintptr_t)((void*)NtCurrentTeb());
#elif !defined(__APPLE__) && !defined(__CYGWIN__) &&                                                \
    ((defined(__clang__) && (__clang_major__ >= 7)) || ((defined(__GNUC__) && (__GNUC__ >= 5)))) && \
    (defined(__aarch64__) || defined(__x86_64__) || defined(__loongarch__))  // Unsure of other archs, needs testing
	void* thp = __builtin_thread_pointer();
	return (uintptr_t)thp;
#else
	uintptr_t tid;
#if defined(__i386__)
	__asm__("movl %%gs:0, %0" : "=r"(tid) : :);
#elif defined(__x86_64__)
#if defined(__MACH__)
	__asm__("movq %%gs:0, %0" : "=r"(tid) : :);
#else
	__asm__("movq %%fs:0, %0" : "=r"(tid) : :);
#endif
#elif defined(__arm__)
	__asm__ volatile("mrc p15, 0, %0, c13, c0, 3" : "=r"(tid));
#elif defined(__aarch64__)
#if defined(__MACH__)
	// tpidr_el0 likely unused, always return 0 on iOS
	__asm__ volatile("mrs %0, tpidrro_el0" : "=r"(tid));
#else
	__asm__ volatile("mrs %0, tpidr_el0" : "=r"(tid));
#endif
#else
	tid = (uintptr_t)&global_thread;
#endif
	return tid;
#endif
}

//! Set the current thread heap
static void
set_thread_heap(heap_t* heap) {
	global_thread.heap = heap;
	if (heap && (heap->id != 0)) {
		rpmalloc_assert(heap->id != 0, "Default heap being used");
		heap->owner_thread = get_thread_id();
	}
#if PLATFORM_WINDOWS
	if (fls_key != FLS_OUT_OF_INDEXES)
		FlsSetValue(fls_key, heap);
#else
	if (pthread_key_created)
		pthread_setspecific(pthread_key, heap);
#endif
}

static heap_t*
get_thread_heap_allocate(void) {
	heap_t* heap = heap_allocate(0);
	set_thread_heap(heap);
	return heap;
}

//! Get the current thread heap
static inline heap_t*
get_thread_heap(void) {
	return global_thread.heap;
}

//! Get the size class from given size in bytes for tiny blocks (below 16 times the minimum granularity)
static inline uint32_t
get_size_class_tiny(size_t size) {
	return (((uint32_t)size + (SMALL_GRANULARITY - 1)) / SMALL_GRANULARITY);
}

//! Get the size class from given size in bytes
static inline uint32_t
get_size_class(size_t size) {
	uintptr_t minblock_count = (size + (SMALL_GRANULARITY - 1)) / SMALL_GRANULARITY;
	// For sizes up to 64 times the minimum granularity (i.e 1024 bytes) the size class is equal to number of such
	// blocks
	if (size <= (SMALL_GRANULARITY * 64)) {
		rpmalloc_assert(global_size_class[minblock_count].block_size >= size, "Size class misconfiguration");
		return (uint32_t)(minblock_count ? minblock_count : 1);
	}
	--minblock_count;
	// Calculate position of most significant bit, since minblock_count now guaranteed to be > 64 this position is
	// guaranteed to be >= 6
	// Use xor rather than subtraction, (N-1) ^ clz == (N-1) - clz for clz in [0, N-1], and compilers fold
	// the xor into the bit scan result (bsr) instead of emitting the clz conversion followed by a subtract
#if ARCH_64BIT
	const uint32_t most_significant_bit = (uint32_t)(63 ^ rpmalloc_clz(minblock_count));
#else
	const uint32_t most_significant_bit = (uint32_t)(31 ^ rpmalloc_clz(minblock_count));
#endif
	// Class sizes are of the bit format [..]000xxx000[..] where we already have the position of the most significant
	// bit, now calculate the subclass from the remaining two bits
	const uint32_t subclass_bits = (minblock_count >> (most_significant_bit - 2)) & 0x03;
	const uint32_t class_idx = (uint32_t)((most_significant_bit << 2) + subclass_bits) + 41;
	rpmalloc_assert((class_idx >= SIZE_CLASS_COUNT) || (global_size_class[class_idx].block_size >= size),
	                "Size class misconfiguration");
	rpmalloc_assert((class_idx >= SIZE_CLASS_COUNT) || (global_size_class[class_idx - 1].block_size < size),
	                "Size class misconfiguration");
	return class_idx;
}

static inline page_type_t
get_page_type(uint32_t size_class) {
	if (size_class < SMALL_SIZE_CLASS_COUNT)
		return PAGE_SMALL;
	else if (size_class < (SMALL_SIZE_CLASS_COUNT + MEDIUM_SMALL_SIZE_CLASS_COUNT))
		return PAGE_MEDIUM_SMALL;
	else if (size_class < (SMALL_SIZE_CLASS_COUNT + MEDIUM_SMALL_SIZE_CLASS_COUNT + MEDIUM_LARGE_SIZE_CLASS_COUNT))
		return PAGE_MEDIUM_LARGE;
	else if (size_class < SIZE_CLASS_COUNT)
		return PAGE_LARGE;
	return PAGE_HUGE;
}

static inline size_t
get_page_aligned_size(size_t size) {
	size_t unalign = size % global_config.page_size;
	if (unalign)
		size += global_config.page_size - unalign;
	return size;
}

//! Size of the mapping of a huge block, in transparent huge page mode a whole number of huge pages if that adds at
//  most an eighth of the size. The last huge page of the mapping would otherwise be backed by small pages, and a
//  cached mapping decommitted from within a huge page and committed again would fault the rest of it in small pages.
//  A size just above a whole number of huge pages, as a power of two size with the block header, keeps its tail in
//  small pages instead of a whole huge page for the few bytes past the boundary
static inline size_t
get_huge_aligned_size(size_t size) {
	if (global_thp_mask & (1U << PAGE_HUGE)) {
		size_t huge_size = (size + (os_thp_size - 1)) & ~(os_thp_size - 1);
		if ((huge_size - size) <= (size >> 3))
			return huge_size;
	}
	return get_page_aligned_size(size);
}

////////////
///
/// OS entry points
///
//////

static void
os_set_page_name(void* address, size_t size) {
#if defined(__linux__) || defined(__ANDROID__)
	const char* name = os_huge_pages ? global_config.huge_page_name : global_config.page_name;
	if ((address == MAP_FAILED) || !name)
		return;
	// If the kernel does not support CONFIG_ANON_VMA_NAME or if the call fails
	// (e.g. invalid name) it is a no-op basically.
	(void)prctl(PR_SET_VMA, PR_SET_VMA_ANON_NAME, (uintptr_t)address, size, (uintptr_t)name);
#else
	(void)sizeof(size);
	(void)sizeof(address);
#endif
}

static void*
os_mmap(size_t size, size_t alignment, size_t* offset, size_t* mapped_size) {
	size_t map_size = size + alignment;
#if PLATFORM_WINDOWS
	// Ok to MEM_COMMIT - according to MSDN, "actual physical pages are not allocated unless/until the virtual addresses
	// are actually accessed". But if we enable decommit it's better to not immediately commit and instead commit per
	// page to avoid saturating the OS commit limit
#if ENABLE_DECOMMIT
	DWORD do_commit = 0;
	if (global_config.disable_decommit)
	    do_commit = MEM_COMMIT;
#else
	DWORD do_commit = MEM_COMMIT;
#endif
	const DWORD base_flags = MEM_RESERVE | do_commit;
	const DWORD large_flag = (os_huge_pages ? MEM_LARGE_PAGES : 0);
	void* ptr = 0;
	if (alignment && os_virtualalloc2) {
		// Map an aligned region directly so no alignment padding is retained per mapping
		MEM_ADDRESS_REQUIREMENTS reqs;
		memset(&reqs, 0, sizeof(reqs));
		reqs.Alignment = alignment;
		MEM_EXTENDED_PARAMETER param;
		memset(&param, 0, sizeof(param));
		param.Type = MemExtendedParameterAddressRequirements;
		param.Pointer = &reqs;
		map_size = size;
		ptr = os_virtualalloc2(0, 0, map_size, large_flag | base_flags, PAGE_READWRITE, &param, 1);
		if (!ptr && large_flag)
			ptr = os_virtualalloc2(0, 0, map_size, base_flags, PAGE_READWRITE, &param, 1);
	} else {
		// Pre Windows 10: over-allocate and offset for alignment below
		ptr = VirtualAlloc(0, map_size, large_flag | base_flags, PAGE_READWRITE);
		if (!ptr && large_flag)
			ptr = VirtualAlloc(0, map_size, base_flags, PAGE_READWRITE);
	}
#else
	int flags = MAP_PRIVATE | MAP_ANONYMOUS | MAP_UNINITIALIZED;
#if defined(__APPLE__) && !TARGET_OS_IPHONE && !TARGET_OS_SIMULATOR
	int fd = (int)VM_MAKE_TAG(240U);
	if (os_huge_pages)
		fd |= VM_FLAGS_SUPERPAGE_SIZE_2MB;
	void* ptr = mmap(0, map_size, PROT_READ | PROT_WRITE, flags, fd, 0);
	if (((ptr == MAP_FAILED) || !ptr) && os_huge_pages) {
		// Superpage allocations can fail due to physical memory fragmentation,
		// fall back to regular pages
		ptr = mmap(0, map_size, PROT_READ | PROT_WRITE, flags, (int)VM_MAKE_TAG(240U), 0);
	}
#elif defined(MAP_HUGETLB)
	int huge_flags = 0;
	if (os_huge_pages) {
		huge_flags = MAP_HUGETLB;
#if defined(MAP_HUGE_SHIFT)
		// Explicitly request the detected huge page size, the system default huge
		// page size can be a different (unusable) size like 1GiB
		if (os_huge_page_shift)
			huge_flags |= (int)(os_huge_page_shift << MAP_HUGE_SHIFT);
#endif
	}
	void* ptr =
	    mmap(0, map_size, PROT_READ | PROT_WRITE | PROT_MAX(PROT_READ | PROT_WRITE), huge_flags | flags, -1, 0);
#if defined(MADV_HUGEPAGE)
	// In some configurations, huge pages allocations might fail thus
	// we fallback to normal allocations and promote the region as transparent huge page
	if ((ptr == MAP_FAILED || !ptr) && os_huge_pages) {
		ptr = mmap(0, map_size, PROT_READ | PROT_WRITE, flags, -1, 0);
		if (ptr && (ptr != MAP_FAILED)) {
			// Best-effort promotion to transparent huge pages; ignore failures (for
			// example when THP is disabled system-wide, where madvise returns EINVAL).
			// The mapping is still valid, just backed by normal pages.
			(void)madvise(ptr, map_size, MADV_HUGEPAGE);
		}
	}
#endif
	os_set_page_name(ptr, map_size);
#elif defined(MAP_ALIGNED)
	const size_t align = (sizeof(size_t) * 8) - (size_t)(__builtin_clzl(size - 1));
	void* ptr = mmap(0, map_size, PROT_READ | PROT_WRITE, (os_huge_pages ? MAP_ALIGNED(align) : 0) | flags, -1, 0);
#elif defined(MAP_ALIGN)
	caddr_t base = (os_huge_pages ? (caddr_t)(4 << 20) : 0);
	void* ptr = mmap(base, map_size, PROT_READ | PROT_WRITE, (os_huge_pages ? MAP_ALIGN : 0) | flags, -1, 0);
#else
	void* ptr = mmap(0, map_size, PROT_READ | PROT_WRITE, flags, -1, 0);
#endif
	if (ptr == MAP_FAILED)
		ptr = 0;
#endif
	if (!ptr) {
		if (global_memory_interface->map_fail_callback) {
			if (global_memory_interface->map_fail_callback(map_size))
				return os_mmap(size, alignment, offset, mapped_size);
		} else {
			rpmalloc_assert(ptr != 0, "Failed to map more virtual memory");
		}
		return 0;
	}
	if (alignment) {
		size_t padding = ((uintptr_t)ptr & (uintptr_t)(alignment - 1));
		if (padding)
			padding = alignment - padding;
		rpmalloc_assert(padding <= alignment, "Internal failure in padding");
		rpmalloc_assert(!(padding % 8), "Internal failure in padding");
#if !PLATFORM_WINDOWS
		// Unmap the alignment padding head and tail to avoid holding on to unusable
		// address space (and huge page reservations when using huge pages). Both the
		// mmap result and the alignment are multiples of the mapping page size, so
		// head and tail stay page aligned (huge page aligned for huge page mappings)
		if (padding)
			munmap(ptr, padding);
		ptr = pointer_offset(ptr, padding);
		if (alignment - padding)
			munmap(pointer_offset(ptr, size), alignment - padding);
		*offset = 0;
		map_size = size;
#else
		ptr = pointer_offset(ptr, padding);
		*offset = padding;
#endif
	}
	*mapped_size = map_size;
#if ENABLE_STATISTICS
	size_t page_count = map_size / global_config.page_size;
	size_t page_mapped_current =
	    atomic_fetch_add_explicit(&global_statistics.page_mapped, page_count, memory_order_relaxed) + page_count;
	size_t page_mapped_peak = atomic_load_explicit(&global_statistics.page_mapped_peak, memory_order_relaxed);
	while (page_mapped_current > page_mapped_peak) {
		if (atomic_compare_exchange_weak_explicit(&global_statistics.page_mapped_peak, &page_mapped_peak,
		                                          page_mapped_current, memory_order_relaxed, memory_order_relaxed))
			break;
	}
#endif
	return ptr;
}

//! Whether newly mapped regions are reserve-only (pages committed on demand). Mirrors the
//  commit choice os_mmap makes; the single definition spans/heaps capture at map time.
static inline int
os_commit_on_demand(void) {
#if ENABLE_DECOMMIT
	return global_config.disable_decommit ? 0 : 1;
#else
	return 0;
#endif
}

static int
os_mcommit(void* address, size_t size) {
#if ENABLE_DECOMMIT
	// Gated by callers on the per-span commit_on_demand flag; spans committed in full at
	// map time (incl. huge/large pages, which cannot be partially committed) never reach here
#if PLATFORM_WINDOWS
	if (!VirtualAlloc(address, size, MEM_COMMIT, PAGE_READWRITE)) {
		if (global_memory_interface->map_fail_callback && global_memory_interface->map_fail_callback(size))
			return os_mcommit(address, size);
		rpmalloc_assert(0, "Failed to commit virtual memory block");
		return 1;
	}
#else
	/*
	if (mprotect(address, size, PROT_READ | PROT_WRITE)) {
		rpmalloc_assert(0, "Failed to commit virtual memory block");
	}
	*/
#if defined(MADV_FREE_REUSE)
	// Pages decommitted with MADV_FREE_REUSABLE must be marked for reuse to be accounted to the process
	// again. Failures are ignored, the pages are accessible either way
	while ((madvise(address, size, MADV_FREE_REUSE) == -1) && (errno == EAGAIN))
		errno = 0;
#endif
#endif
#if ENABLE_STATISTICS
	size_t page_count = size / global_config.page_size;
	atomic_fetch_add_explicit(&global_statistics.page_commit, page_count, memory_order_relaxed);
	size_t page_active_current =
	    atomic_fetch_add_explicit(&global_statistics.page_active, page_count, memory_order_relaxed) + page_count;
	size_t page_active_peak = atomic_load_explicit(&global_statistics.page_active_peak, memory_order_relaxed);
	while (page_active_current > page_active_peak) {
		if (atomic_compare_exchange_weak_explicit(&global_statistics.page_active_peak, &page_active_peak,
		                                          page_active_current, memory_order_relaxed, memory_order_relaxed))
			break;
	}
#endif
#endif
	(void)sizeof(address);
	(void)sizeof(size);
	return 0;
}

//! Set if pages decommitted by os_mdecommit read as zero once committed again: Windows decommits them and Linux
//  discards private anonymous pages with MADV_DONTNEED. Other platforms may keep the old content (Apple and the
//  BSDs only advise the kernel that the pages can be reclaimed)
#if PLATFORM_WINDOWS || defined(__linux__) || defined(__ANDROID__)
#define OS_DECOMMIT_ZEROES 1
#else
#define OS_DECOMMIT_ZEROES 0
#endif

static int
os_mdecommit(void* address, size_t size) {
#if ENABLE_DECOMMIT
	// Unlike os_mcommit, honor the live flag so the Linux fallback below can stop all
	// further decommits by setting disable_decommit mid-run
	if (global_config.disable_decommit)
		return 1;
#if PLATFORM_WINDOWS
	if (!VirtualFree(address, size, MEM_DECOMMIT)) {
		rpmalloc_assert(0, "Failed to decommit virtual memory block");
		return 1;
	}
#else
	/*
	if (mprotect(address, size, PROT_NONE)) {
		rpmalloc_assert(0, "Failed to decommit virtual memory block");
	}
	*/
#if defined(MADV_FREE_REUSABLE)
	// Apple platforms only deactivate pages given MADV_DONTNEED, which neither lowers the resident size
	// nor the memory footprint of the process, while touching the pages again still page faults.
	// Reusable pages are taken off the footprint right away and are reclaimed under memory pressure
	int ret;
	while ((ret = madvise(address, size, MADV_FREE_REUSABLE)) == -1 && (errno == EAGAIN))
		errno = 0;
	if ((ret == -1) && (errno != 0)) {
#elif defined(MADV_DONTNEED)
	if (madvise(address, size, MADV_DONTNEED)) {
#elif defined(MADV_PAGEOUT)
	if (madvise(address, size, MADV_PAGEOUT)) {
#elif defined(MADV_FREE)
	if (madvise(address, size, MADV_FREE)) {
#else
	if (posix_madvise(address, size, POSIX_MADV_DONTNEED)) {
#endif
#if defined(__linux__)
		if (os_huge_pages) {
			// MADV_DONTNEED on huge page mappings requires Linux 5.18 or later, keep
			// pages committed and stop further decommit attempts
			global_config.disable_decommit = 1;
			return 1;
		}
#endif
		rpmalloc_assert(0, "Failed to decommit virtual memory block");
		return 1;
	}
#endif
#if ENABLE_STATISTICS
	size_t page_count = size / global_config.page_size;
	atomic_fetch_add_explicit(&global_statistics.page_decommit, page_count, memory_order_relaxed);
	statistics_sub_saturating(&global_statistics.page_active, page_count);
#endif
#else
	(void)sizeof(address);
	(void)sizeof(size);
#endif
	return 0;
}

static void
os_munmap(void* address, size_t offset, size_t mapped_size) {
	(void)sizeof(mapped_size);
	address = pointer_offset(address, -(int32_t)offset);
#if ENABLE_UNMAP
#if PLATFORM_WINDOWS
	if (!VirtualFree(address, 0, MEM_RELEASE)) {
		rpmalloc_assert(0, "Failed to unmap virtual memory block");
	}
#else
	if (munmap(address, mapped_size))
		rpmalloc_assert(0, "Failed to unmap virtual memory block");
#endif
#if ENABLE_STATISTICS
	size_t page_count = mapped_size / global_config.page_size;
	statistics_sub_saturating(&global_statistics.page_mapped, page_count);
	statistics_sub_saturating(&global_statistics.page_active, page_count);
#endif
#endif
}

//! Map memory through the memory interface, which must report the mapped size covering the offset and the requested
//  size, since huge block mappings are reused and grown within the mapped region
static inline void*
memory_interface_map(size_t size, size_t alignment, size_t* offset, size_t* mapped_size) {
	void* ptr = global_memory_interface->memory_map(size, alignment, offset, mapped_size);
	rpmalloc_assert(!ptr || (*mapped_size >= (*offset + size)), "Memory map did not report the mapped size");
	return ptr;
}

////////////
///
/// Page interface
///
//////

static inline span_t*
page_get_span(page_t* page) {
	return (span_t*)((uintptr_t)page & SPAN_MASK);
}

//! Get the slot table entry of the slot holding the given address, computed from the address alone
static inline page_slot_t*
block_get_slot(const void* block) {
	page_slot_t* slot_table = (page_slot_t*)((uintptr_t)block & SPAN_MASK);
#if defined(__GNUC__) || defined(__clang__)
	// Keep the compiler from merging the span start and the entry offset into one bitwise or, a four
	// instruction sequence the AArch64 machine outliner moves out of the free fast path into a call
	__asm__("" : "+r"(slot_table));
#endif
	return slot_table + (((uintptr_t)block >> SPAN_SLOT_SHIFT) & (SPAN_SLOT_COUNT - 1));
}

//! Get the page holding the given block
static inline page_t*
block_get_page(const void* block) {
	return block_get_slot(block)->page;
}

//! Get the page structure of the page with the given index in a span of pages
static inline page_t*
span_get_page(span_t* span, uint32_t page_index) {
	return (page_t*)pointer_offset(span, SPAN_PAGE_ARRAY_OFFSET + ((size_t)page_index * PAGE_HEADER_SIZE));
}

//! Get the index of the first page of a span of pages, the pages before it hold the span metadata
static inline uint32_t
span_first_page(span_t* span) {
	return (span->page_type == PAGE_SMALL) ? SPAN_SMALL_FIRST_PAGE : SPAN_FIRST_PAGE;
}

//! Get the page structure of a huge block span, which follows the span header
static inline page_t*
span_get_huge_page(span_t* span) {
	return (page_t*)pointer_offset(span, SPAN_HEADER_SIZE);
}

//! Map the slots a huge block can start in to the huge block page, which takes the generic free path
static inline void
span_set_huge_slots(span_t* span) {
	page_t* page = span_get_huge_page(span);
	for (uint32_t islot = 0; islot < SPAN_HEAD_SLOT_COUNT; ++islot) {
		span->slot_head[islot].page = page;
		atomic_store_explicit(&span->slot_head[islot].key, PAGE_KEY_GENERIC, memory_order_relaxed);
	}
}

//! Fast free key of a page owned by the given heap
static inline uintptr_t
page_key(heap_t* heap) {
	return (uintptr_t)heap | 1;
}

//! Set the fast free key in the slot table entries of every slot of the page. A small page has a single slot,
//  the larger page types 16, 64 and 256
static inline void
page_set_key(page_t* page, uintptr_t key) {
	page_slot_t* slot = block_get_slot(page->block_start);
	if (EXPECTED(page->page_type == PAGE_SMALL)) {
		atomic_store_explicit(&slot->key, key, memory_order_relaxed);
		return;
	}
	uint32_t slot_count = 1U << (2 + (2 * (uint32_t)page->page_type));
	for (uint32_t islot = 0; islot < slot_count; ++islot)
		atomic_store_explicit(&slot[islot].key, key, memory_order_relaxed);
}

static inline size_t
page_get_size(page_t* page) {
	if (page->page_type == PAGE_SMALL)
		return SMALL_PAGE_SIZE;
	else if (page->page_type == PAGE_MEDIUM_SMALL)
		return MEDIUM_SMALL_PAGE_SIZE;
	else if (page->page_type == PAGE_MEDIUM_LARGE)
		return MEDIUM_LARGE_PAGE_SIZE;
	else if (page->page_type == PAGE_LARGE)
		return LARGE_PAGE_SIZE;
	else
		return page_get_span(page)->page_size;
}

#ifndef PAGE_COMMIT_PART_SHIFT
//! A page committing on demand commits its memory as its blocks are initialized, first a part of
//  1/2^PAGE_COMMIT_PART_SHIFT of the page, of at least a small page and the commit unit, then at least doubling
//  the committed memory each time. A page of large blocks most often holds a few of them, committing all of it up
//  front charged the process commit for memory it never touched, and the doubling bounds the commit calls of a page
//  filled up to PAGE_COMMIT_PART_SHIFT + 1
#define PAGE_COMMIT_PART_SHIFT 4
#endif

//! Size of the parts a page of the given size commits its memory in
static inline size_t
page_commit_part_size(size_t page_size, uint32_t commit_shift) {
	size_t part = page_size >> PAGE_COMMIT_PART_SHIFT;
	if (part < SMALL_PAGE_SIZE)
		part = SMALL_PAGE_SIZE;
	if (part < ((size_t)1 << commit_shift))
		part = (size_t)1 << commit_shift;
	return (part < page_size) ? part : page_size;
}

#if ENABLE_DECOMMIT
//! Commit the memory of a page committing on demand up to at least the given offset from the page start, in whole
//  parts and at least doubling the committed memory. Returns non-zero if the memory could not be committed
static NOINLINE int
page_commit_extend(page_t* page, size_t end) {
	size_t page_size = page_get_size(page);
	size_t part = page_commit_part_size(page_size, page->commit_shift);
	size_t commit_size = page->commit_size;
	size_t commit_end = (end + (part - 1)) & ~(part - 1);
	if (commit_end < (commit_size << 1))
		commit_end = commit_size << 1;
	if (commit_end > page_size)
		commit_end = page_size;
	// The page is in the available list and can have no used block. Counted as used across the commit, so a collect
	// from the commit, as by the map fail callback of the default commit, does not retire it under the allocation
	++page->block_used;
	int failed = global_memory_interface->memory_commit(pointer_offset(page->block_start, commit_size),
	                                                    commit_end - commit_size);
	--page->block_used;
	if (failed)
		return 1;
#if RPMALLOC_HEAP_STATISTICS
	page->heap->stats.committed_size += commit_end - commit_size;
#endif
	page->commit_size = (uint32_t)commit_end;
	return 0;
}
#endif

//! Check if the page is owned by the calling thread. Compares the owning heap with the thread heap
//  rather than loading heap->owner_thread: the thread heap load does not depend on the page header,
//  so the check is one dependent load shorter, and a cross-thread free no longer reads the first
//  cache line of the owning heap, which the owner writes on every small block allocation.
static inline int
page_is_thread_heap(page_t* page) {
#if RPMALLOC_FIRST_CLASS_HEAPS
	return ((page->heap == get_thread_heap()) || !page->heap->owner_thread);
#else
	return (page->heap == get_thread_heap());
#endif
}

static inline block_t*
page_block(page_t* page, uint32_t block_index) {
	return pointer_offset(page->block_start, page->block_size * block_index);
}

//! Byte offset of a block from the start of the first block of its page. Pages are at most LARGE_PAGE_SIZE
//  (16MiB), so the offset always fits in 32 bits.
static inline uint32_t
page_block_offset(page_t* page, block_t* block) {
	return (uint32_t)pointer_diff(block, page->block_start);
}

//! Set in the thread free token of a full page, whose list is empty
#define THREAD_FREE_FULL (1ULL << 63ULL)
//! Set in the thread free token of a page in the pending page list of its heap. A cross-thread free leaving
//  every used block of the page in the list queues the page, which the heap retires at its next allocation
//  slow path instead of keeping the memory until it allocates from the page again. The heap clears the flag
//  when it takes the page from the list, and a queued page is not retired otherwise
#define THREAD_FREE_QUEUED (1ULL << 62ULL)
//! Mask of the list count in the thread free token
#define THREAD_FREE_COUNT_MASK 0x3FFFFFFFULL

//! The thread free list token holds the list head as a byte offset from the first block in the low
//  32 bits (a block index would cost a division on every cross-thread free and a multiplication
//  on every decode), the list count in the next 30 bits, the queued flag and the full page flag in the top bit
static inline uint32_t
page_block_from_thread_free_list(page_t* page, uint64_t token, block_t** block) {
	uint32_t block_offset = (uint32_t)(token & 0xFFFFFFFFULL);
	uint32_t list_count = (uint32_t)((token >> 32ULL) & THREAD_FREE_COUNT_MASK);
	*block = list_count ? (block_t*)pointer_offset(page->block_start, block_offset) : 0;
	return list_count;
}

static inline uint64_t
page_block_to_thread_free_list(page_t* page, uint32_t block_offset, uint32_t list_count) {
	(void)sizeof(page);
	return ((uint64_t)list_count << 32ULL) | (uint64_t)block_offset;
}

static inline block_t*
page_block_realign(page_t* page, block_t* block) {
	uint32_t block_offset = page_block_offset(page, block);
	return pointer_offset(block, -(int32_t)(block_offset % page->block_size));
}

static block_t*
page_get_local_free_block(page_t* page) {
	block_t* block = page->local_free;
	page->local_free = block->next;
	++page->block_used;
	return block;
}

static inline void
page_decommit_memory_pages(page_t* page) {
	if (page->is_decommitted)
		return;
	// Only reserve-only spans support decommit
	if (!page->commit_on_demand)
		return;
	// The page structure is kept in the span metadata, so all of the page memory is decommitted. The page
	// must be a whole number of commit units, the span's fixed map-time page size
	size_t page_size = page_get_size(page);
	if (page_size < ((size_t)1 << page->commit_shift))
		return;
	// Only the committed part of the page memory is decommitted, the rest was never committed
	size_t commit_size = page->commit_size;
	if (commit_size && (global_memory_interface->memory_decommit(page->block_start, commit_size) != 0))
		return;
#if RPMALLOC_HEAP_STATISTICS && ENABLE_DECOMMIT
	if (page->heap)
		page->heap->stats.committed_size -= commit_size;
#endif
	page->commit_size = 0;
	page->is_decommitted = 1;
}

//! Make a decommitted page usable again. Its memory is committed in parts as its blocks are initialized
static inline int
page_commit_memory_pages(page_t* page) {
	if (!page->is_decommitted)
		return 0;
	page->is_decommitted = 0;
#if ENABLE_DECOMMIT
#if OS_DECOMMIT_ZEROES
	// All of the page memory was decommitted, so the OS zero fills it when recommitted - take advantage in
	// zalloc/calloc calls. A custom memory interface may keep the content of decommitted pages
	if (global_memory_interface->memory_decommit == os_mdecommit)
		page->is_zero = 1;
#endif
#endif
	return 0;
}

//! Unlink a page that is not the head from the available list. The head prev pointer is the tail
static inline void
page_available_unlink(heap_t* heap, page_t* page) {
	page->prev->next = page->next;
	if (page->next)
		page->next->prev = page->prev;
	else
		heap->page_available[page->size_class]->prev = page->prev;
}

//! Move the head page of the available list to the tail
static inline void
page_available_rotate(heap_t* heap, page_t* page) {
	page_t* tail = page->prev;
	page_t* head = page->next;
	heap->page_available[page->size_class] = head;
	head->prev = page;
	tail->next = page;
	page->prev = tail;
	page->next = 0;
}

static FORCEINLINE void
heap_page_free(heap_t* heap, page_t* page);

static void
page_available_to_free(page_t* page) {
	rpmalloc_assert(page->is_full == 0, "Page full flag internal failure");
	rpmalloc_assert(page->is_decommitted == 0, "Page decommitted flag internal failure");
	// A queued page is retired when the heap takes it from the pending list
	if (UNEXPECTED(atomic_load_explicit(&page->thread_free, memory_order_relaxed) & THREAD_FREE_QUEUED))
		return;
	heap_t* heap = page->heap;
	++heap->activity;
	if (heap->page_available[page->size_class] == page) {
		// Keep the last available page of a size class in place. Retiring it would make the next
		// allocation of the class take the generic path and reinitialize the page from scratch,
		// which an alternating allocate/free pattern would then repeat on every pair
		if (!page->next)
			return;
		heap->page_available[page->size_class] = page->next;
		page->next->prev = page->prev;
	} else {
		page_available_unlink(heap, page);
	}
	heap_page_free(heap, page);
}

//! Put a page without used blocks, unlinked from the available list, in the free pages of the heap. Inlined in the
//  free path, the compilers moved it out of line once the collect also called it
static FORCEINLINE void
heap_page_free(heap_t* heap, page_t* page) {
	page->is_free = 1;
	page->is_zero = 0;
	page->next = heap->page_free[page->page_type];
	heap->page_free[page->page_type] = page;
	if (!page->next)
		heap->page_free_tail[page->page_type] = page;
	if (++heap->page_free_commit_count[page->page_type] > global_page_free_retain[page->page_type])
		heap_page_free_overflow(heap, page->page_type);
	if (heap_page_free_overflow_other_pending(heap, page->page_type))
		heap_page_free_overflow_other(heap, page->page_type);
}

static void
page_full_to_available(page_t* page) {
	rpmalloc_assert(page->is_full == 1, "Page full flag internal failure");
	rpmalloc_assert(page->is_decommitted == 0, "Page decommitted flag internal failure");
	heap_t* heap = page->heap;
	page_t* head = heap->page_available[page->size_class];
	page->next = head;
	page->prev = head ? head->prev : page;
	if (head)
		head->prev = page;
	heap->page_available[page->size_class] = page;
	page->is_full = 0;
	if (page->has_aligned_block == 0)
		page_set_key(page, page_key(heap));
	// Other threads do not modify the token of a page flagged full, it holds only the flags
	unsigned long long token = atomic_load_explicit(&page->thread_free, memory_order_relaxed);
	atomic_store_explicit(&page->thread_free, token & THREAD_FREE_QUEUED, memory_order_release);
}

static void
page_full_to_free_on_new_heap(page_t* page, heap_t* heap) {
	rpmalloc_assert(heap->id, "Page full to free on default heap");
	rpmalloc_assert(page->is_full == 1, "Page full flag internal failure");
	rpmalloc_assert(page->is_decommitted == 0, "Page decommitted flag internal failure");
	page->is_full = 0;
	page->is_free = 1;
	page->heap = heap;
	atomic_store_explicit(&page->thread_free, 0, memory_order_release);
	page->next = heap->page_free[page->page_type];
	heap->page_free[page->page_type] = page;
	if (!page->next)
		heap->page_free_tail[page->page_type] = page;
	if (++heap->page_free_commit_count[page->page_type] > global_page_free_retain[page->page_type])
		heap_page_free_overflow(heap, page->page_type);
}

static void
page_available_to_full(page_t* page) {
	heap_t* heap = page->heap;
	if (heap->page_available[page->size_class] == page) {
		heap->page_available[page->size_class] = page->next;
		if (page->next)
			page->next->prev = page->prev;
	} else {
		page_available_unlink(heap, page);
	}
	page->is_full = 1;
	page->is_zero = 0;
	page_set_key(page, PAGE_KEY_GENERIC);
}

static inline void
page_put_local_free_block(page_t* page, block_t* block) {
	block->next = page->local_free;
	page->local_free = block;
	_rpmalloc_stat_add_free(page->heap, page->size_class, 1);
	if (UNEXPECTED(--page->block_used == 0)) {
		page_available_to_free(page);
	} else if (UNEXPECTED(page->is_full != 0)) {
		page_full_to_available(page);
	}
}

//! Put a run of count blocks of the page, linked from first to last, freed by other threads and taken from
//  the heap level thread free list in the page free list at once. The freeing threads have already
//  subtracted the blocks from the heap allocated size statistics
static void
page_put_thread_free_run(page_t* page, block_t* first, block_t* last, uint32_t count) {
	last->next = page->local_free;
	page->local_free = first;
	page->remote_fed = 1;
	_rpmalloc_stat_add_free(page->heap, page->size_class, count);
	rpmalloc_assert(count <= page->block_used, "Page block use counter out of sync");
	page->block_used -= count;
	if (page->is_full)
		page_full_to_available(page);
	if (page->block_used == 0)
		page_available_to_free(page);
}

static NOINLINE void
page_adopt_thread_free_block_list(page_t* page) {
	if (page->local_free)
		return;
	unsigned long long thread_free = atomic_load_explicit(&page->thread_free, memory_order_relaxed);
	if (thread_free & ~THREAD_FREE_QUEUED) {
		test_yield_point();
		// Other threads can only replace with another valid list head, the queued flag stays until the heap takes
		// the page from the pending list
		while (!atomic_compare_exchange_weak_explicit(&page->thread_free, &thread_free,
		                                              thread_free & THREAD_FREE_QUEUED, memory_order_acquire,
		                                              memory_order_relaxed)) {
		}
		page->remote_fed = 1;
		uint32_t thread_free_count = page_block_from_thread_free_list(page, thread_free, &page->local_free);
		rpmalloc_assert(thread_free_count <= page->block_used, "Page thread free list count internal failure");
		page->block_used -= thread_free_count;
		_rpmalloc_stat_add_free(page->heap, page->size_class, thread_free_count);
	}
}

//! Contended retry path for pushing a block to the heap level thread free list of full pages
static NOINLINE void
heap_put_thread_free_block_retry(atomic_uintptr_t* list, block_t* block, uintptr_t prev_head) {
	uint32_t spin = 0;
	do {
		block->next = (void*)prev_head;
		wait_spin(&spin);
	} while (!atomic_compare_exchange_weak_explicit(list, &prev_head, (uintptr_t)block, memory_order_release,
	                                                memory_order_relaxed));
}

static int
page_orphan_full(page_t* page, heap_t* heap, block_t* head, block_t** tail_link, uint32_t count);

static NOINLINE void
page_put_thread_free_block(page_t* page, block_t* block);

static NOINLINE void
page_put_thread_free_chain(page_t* page, block_t* head, block_t** tail_link, uint32_t count);

//! Heap owning the orphan pages, which no thread owns (see the orphan lists)
static RPMALLOC_CACHE_ALIGNED heap_t global_heap_orphan;

//! Push a block freed into a full page to the heap level thread free list, otherwise the heap would
//  not pick up the block until a thread local free happens. A page another thread orphaned since the full flag was
//  read is no longer full, and the block goes to its thread free list instead, the orphan heap collects no list
static NOINLINE void
heap_put_thread_free_block(page_t* page, block_t* block) {
	test_full_free_delay();
	// The owner wrote the heap of a page it took from the orphan lists before it flagged the page full
	atomic_thread_fence(memory_order_acquire);
	heap_t* heap = page->heap;
	if (UNEXPECTED(heap == &global_heap_orphan)) {
		page_put_thread_free_block(page, block);
		return;
	}
	if (UNEXPECTED(atomic_load_explicit(&heap->orphaned, memory_order_relaxed))) {
		int orphaned = page_orphan_full(page, heap, block, &block->next, 1);
		if (orphaned > 0)
			return;
		if (orphaned < 0) {
			page_put_thread_free_block(page, block);
			return;
		}
	}
	atomic_uintptr_t* list = &heap->thread_free[page->page_type];
	uintptr_t prev_head = atomic_load_explicit(list, memory_order_relaxed);
	block->next = (void*)prev_head;
	if (UNEXPECTED(!atomic_compare_exchange_weak_explicit(list, &prev_head, (uintptr_t)block, memory_order_release,
	                                                      memory_order_relaxed)))
		heap_put_thread_free_block_retry(list, block, prev_head);
}

//! Push a page to the pending page list of its heap. The page has the queued flag set, so it is not retired and
//  stays owned by the heap until the heap takes it from the list
static NOINLINE void
heap_push_pending_page(page_t* page) {
	test_pending_push_delay();
	// A heap taking the page from the orphan lists wrote the heap before it cleared the queued flag
	atomic_thread_fence(memory_order_acquire);
	heap_t* heap = page->heap;
	uintptr_t head = atomic_load_explicit(&heap->page_pending, memory_order_relaxed);
	do {
		page->pending_next = (page_t*)head;
	} while (!atomic_compare_exchange_weak_explicit(&heap->page_pending, &head, (uintptr_t)page, memory_order_release,
	                                                memory_order_relaxed));
}

//! Thread free token of a push making the list hold the given head and list size, queuing the page when the list
//  then holds every used block of the page. The used block count is written by the owner without synchronization,
//  a stale count queues a page the heap does not retire or leaves a page to be adopted from as before
static inline uint64_t
page_thread_free_token(page_t* page, uint64_t prev_thread_free, uint32_t head_offset, uint32_t list_size) {
	uint64_t token =
	    page_block_to_thread_free_list(page, head_offset, list_size) | (prev_thread_free & THREAD_FREE_QUEUED);
	if (list_size == *(volatile uint32_t*)&page->block_used)
		token |= THREAD_FREE_QUEUED;
	return token;
}

//! Contended retry path for pushing a block to the page thread free list, or to the heap level list
//  once the owner has flagged the page full
static NOINLINE void
page_put_thread_free_block_retry(page_t* page, block_t* block, uint32_t block_offset,
                                 unsigned long long prev_thread_free) {
	uint32_t spin = 0;
	uint64_t thread_free;
	do {
		if (prev_thread_free & THREAD_FREE_FULL) {
			heap_put_thread_free_block(page, block);
			return;
		}
		uint32_t list_size = page_block_from_thread_free_list(page, prev_thread_free, &block->next) + 1;
		thread_free = page_thread_free_token(page, prev_thread_free, block_offset, list_size);
		wait_spin(&spin);
	} while (!atomic_compare_exchange_weak_explicit(&page->thread_free, &prev_thread_free, thread_free,
	                                                memory_order_release, memory_order_relaxed));
	if ((thread_free & THREAD_FREE_QUEUED) && !(prev_thread_free & THREAD_FREE_QUEUED))
		heap_push_pending_page(page);
}

//! Push a block to the deferred free list of another thread. The uncontended case is a single CAS,
//  the spin and yield retry loops live in separate cold functions so this path needs no stack frame
//  or callee saved registers. The owner flags a full page in the thread free token, blocks freed into
//  it go to the heap level list instead
static NOINLINE void
page_put_thread_free_block(page_t* page, block_t* block) {
	unsigned long long prev_thread_free = atomic_load_explicit(&page->thread_free, memory_order_relaxed);
	if (UNEXPECTED(prev_thread_free & THREAD_FREE_FULL)) {
		heap_put_thread_free_block(page, block);
		return;
	}
	uint32_t block_offset = page_block_offset(page, block);
	rpmalloc_assert(!(block_offset % page->block_size), "Block pointer is not aligned to start of block");
	uint32_t list_size = page_block_from_thread_free_list(page, prev_thread_free, &block->next) + 1;
	uint64_t thread_free = page_thread_free_token(page, prev_thread_free, block_offset, list_size);
	test_yield_point();
	if (UNEXPECTED(!atomic_compare_exchange_weak_explicit(&page->thread_free, &prev_thread_free, thread_free,
	                                                      memory_order_release, memory_order_relaxed))) {
		page_put_thread_free_block_retry(page, block, block_offset, prev_thread_free);
		return;
	}
	if (UNEXPECTED((thread_free & THREAD_FREE_QUEUED) && !(prev_thread_free & THREAD_FREE_QUEUED)))
		heap_push_pending_page(page);
}

//! Push a chain of blocks freed into a full page to the heap level thread free list, the chain is spliced
//  in front of the list by storing the list head in the next pointer of the last chained block
static void
heap_put_thread_free_chain(page_t* page, block_t* head, block_t** tail_link, uint32_t count) {
	test_full_free_delay();
	// The owner wrote the heap of a page it took from the orphan lists before it flagged the page full
	atomic_thread_fence(memory_order_acquire);
	heap_t* heap = page->heap;
	if (UNEXPECTED(heap == &global_heap_orphan)) {
		page_put_thread_free_chain(page, head, tail_link, count);
		return;
	}
	if (UNEXPECTED(atomic_load_explicit(&heap->orphaned, memory_order_relaxed))) {
		int orphaned = page_orphan_full(page, heap, head, tail_link, count);
		if (orphaned > 0)
			return;
		if (orphaned < 0) {
			page_put_thread_free_chain(page, head, tail_link, count);
			return;
		}
	}
	atomic_uintptr_t* list = &heap->thread_free[page->page_type];
	uintptr_t prev_head = atomic_load_explicit(list, memory_order_relaxed);
	uint32_t spin = 0;
	while (1) {
		*tail_link = (void*)prev_head;
		if (atomic_compare_exchange_weak_explicit(list, &prev_head, (uintptr_t)head, memory_order_release,
		                                          memory_order_relaxed))
			return;
		wait_spin(&spin);
	}
}

//! Push a chain of count blocks freed by this thread into a page owned by another heap with one compare
//  and swap, to the page thread free list, or to the heap level list once the owner has flagged the page
//  full. The list head is stored in the next pointer of the last chained block, and the token takes the
//  chain head and the summed count
static NOINLINE void
page_put_thread_free_chain(page_t* page, block_t* head, block_t** tail_link, uint32_t count) {
	unsigned long long prev_thread_free = atomic_load_explicit(&page->thread_free, memory_order_relaxed);
	uint32_t head_offset = page_block_offset(page, head);
	uint32_t spin = 0;
	while (1) {
		if (prev_thread_free & THREAD_FREE_FULL) {
			heap_put_thread_free_chain(page, head, tail_link, count);
			return;
		}
		uint32_t list_size = page_block_from_thread_free_list(page, prev_thread_free, tail_link) + count;
		uint64_t thread_free = page_thread_free_token(page, prev_thread_free, head_offset, list_size);
		test_yield_point();
		if (atomic_compare_exchange_weak_explicit(&page->thread_free, &prev_thread_free, thread_free,
		                                          memory_order_release, memory_order_relaxed)) {
			if ((thread_free & THREAD_FREE_QUEUED) && !(prev_thread_free & THREAD_FREE_QUEUED))
				heap_push_pending_page(page);
			return;
		}
		wait_spin(&spin);
	}
}

//! Push the blocks held in the calling thread's free chain to their page. The chain page is kept, a following
//  free into the page begins a new chain
static NOINLINE void
thread_free_chain_flush(thread_context_t* thread) {
	uint32_t count = thread->chain_count;
	if (!count)
		return;
	block_t** tail_link = thread->chain_link;
	thread->chain_count = 0;
	thread->chain_link = &thread->chain_head;
	page_put_thread_free_chain(thread->chain_page, thread->chain_head, tail_link, count);
}

//! Flush the calling thread's free chain and stop batching, for a thread leaving its heap
static void
thread_free_chain_release(void) {
	thread_context_t* thread = &global_thread;
	thread_free_chain_flush(thread);
	thread->chain_page = 0;
}

#if defined(__APPLE__)
//! Thread local variable destructor pushing the free chain of an exiting thread. Runs while the thread
//  local storage is still valid, unlike the thread exit destructor
static void
thread_free_chain_exit(void* context) {
	thread_context_t* thread = context;
	thread_free_chain_flush(thread);
	thread->chain_page = 0;
	thread->chain_exit = CHAIN_EXIT_FINALIZED;
}
#endif

//! Make sure the chain of the calling thread is pushed when the thread exits, returns non-zero if it is and
//  the thread can batch. The thread exit destructor finalizes a thread with a heap, which pushes the chain.
//  A thread that has not allocated has no heap and no allocation slow path, and the destructor only runs
//  for a thread with a heap. Threads that only free are common (consumers), so such a thread registers the
//  destructor with the fallback heap as value, the value a finalized thread leaves. Once the thread has
//  finalized it stops batching until it has a heap again, the destructor may not run another time. On
//  macOS thread local storage is reinitialized before the thread exit destructor runs, which then sees an
//  empty chain, so every batching thread registers a thread local variable destructor instead. Once the
//  module is unloaded the destructor is released and the thread does not batch
static NOINLINE int
thread_free_chain_register(thread_context_t* thread) {
	if (thread->chain_exit == CHAIN_EXIT_UNREGISTERED) {
#if defined(__APPLE__)
		_tlv_atexit(thread_free_chain_exit, thread);
#elif PLATFORM_WINDOWS
		if (fls_key == FLS_OUT_OF_INDEXES)
			return 0;
		FlsSetValue(fls_key, &global_heap_fallback);
#else
		if (!pthread_key_created)
			return 0;
		pthread_setspecific(pthread_key, &global_heap_fallback);
#endif
		thread->chain_exit = CHAIN_EXIT_REGISTERED;
	}
	return (thread->chain_exit == CHAIN_EXIT_REGISTERED);
}

//! Begin a chain with the second free in a row into the chain page, the first one was pushed at once and a run
//  of one free is never delayed. Batching is limited to small and medium small pages to bound the memory a
//  thread can hold back, and to pages of heaps that are not first class. A thread with a null heap does not
//  batch, it has no flush points, nor does a thread with its heap idle, which has none until it takes the heap back
//  and would hold the blocks back from the reaps. Otherwise the chain page is cleared and the block pushed
static NOINLINE void
thread_free_chain_begin(page_t* page, block_t* block) {
	thread_context_t* thread = &global_thread;
	heap_t* heap = thread->heap;
	int batch = (page->page_type <= PAGE_MEDIUM_SMALL) && heap && !thread->idle_heap;
#if RPMALLOC_FIRST_CLASS_HEAPS
	batch = batch && !page->heap_first_class;
#endif
	if (batch) {
#if defined(__APPLE__)
		int exit_push = (thread->chain_exit == CHAIN_EXIT_REGISTERED);
#else
		int exit_push = heap->id || (thread->chain_exit == CHAIN_EXIT_REGISTERED);
#endif
		if (UNEXPECTED(!exit_push))
			batch = thread_free_chain_register(thread);
	}
	if (!batch) {
		thread->chain_page = 0;
		page_put_thread_free_block(page, block);
		return;
	}
	rpmalloc_assert(!(page_block_offset(page, block) % page->block_size),
	                "Block pointer is not aligned to start of block");
	thread->chain_head = block;
	thread->chain_link = &block->next;
	thread->chain_count = 1;
#if THREAD_FREE_CHAIN_LIMIT_BYTES
	if (UNEXPECTED(page->block_size >= THREAD_FREE_CHAIN_LIMIT_BYTES))
		thread_free_chain_flush(thread);
#endif
}

//! A cross-thread free into another page than the chain page ending a run of chained frees
static NOINLINE void
thread_free_chain_switch_flush(page_t* page, block_t* block) {
	thread_context_t* thread = &global_thread;
	thread_free_chain_flush(thread);
	thread->chain_page = page;
	page_put_thread_free_block(page, block);
}

static void
page_push_local_free_to_heap(page_t* page) {
	// Push the page free list as the fast track list of free blocks for heap
	// Every initialized block not counted as used is in the local free list, so after handing the
	// whole list to the heap all initialized blocks are in use
	++page->heap->activity;
	page->heap->local_free[page->size_class] = page->local_free;
	page->block_used = page->block_initialized;
	page->local_free = 0;
}

static NOINLINE void*
page_initialize_blocks(page_t* page) {
	rpmalloc_assert(page->block_initialized < page->block_count, "Block initialization internal failure");
#if ENABLE_DECOMMIT
	// Blocks are initialized in address order, the committed part of the page grows ahead of them. A small
	// page commits all of its memory with its first block, the free list linked below stays within it
	size_t block_end = ((size_t)page->block_initialized + 1) * page->block_size;
	if (UNEXPECTED(block_end > page->commit_size) && (page_commit_extend(page, block_end) != 0))
		return 0;
#endif
	block_t* block = page_block(page, page->block_initialized);
	++page->block_initialized;
	++page->block_used;

	if ((page->page_type == PAGE_SMALL) && (page->block_size < (global_config.page_size >> 1))) {
		// Link up until next memory page in free list
		void* memory_page_start = (void*)((uintptr_t)block & ~(uintptr_t)(global_config.page_size - 1));
		void* memory_page_next = pointer_offset(memory_page_start, global_config.page_size);
		block_t* free_block = pointer_offset(block, page->block_size);
		block_t* first_block = free_block;
		block_t* last_block = free_block;
		uint32_t list_count = 0;
		uint32_t max_list_count = page->block_count - page->block_initialized;
		while (((void*)free_block < memory_page_next) && (list_count < max_list_count)) {
			last_block = free_block;
			free_block->next = pointer_offset(free_block, page->block_size);
			free_block = free_block->next;
			++list_count;
		}
		if (list_count) {
			last_block->next = 0;
			page->local_free = first_block;
			page->block_initialized += list_count;
		}
	}

	return block;
}

//! Allocate a block from the page, returns null if the page has no free block left. The page is only
//  marked full here, when a refill finds it without free blocks, not when its free list moves to the
//  heap free list: a page whose free blocks sit in the heap free list is still in use, and marking it
//  full then made the next local free into it take the generic path to make it available again. The
//  first zero_size bytes of the block are zeroed
static inline RPMALLOC_ALLOCATOR void*
page_allocate_block(page_t* page, size_t zero_size) {
	unsigned int is_zero = 0;
	block_t* block = 0;
	if (page->local_free != 0) {
		// Fresh blocks never stay in the page local free list across refills, so these were freed by the
		// owner thread
		page->local_reuse = 1;
		block = page_get_local_free_block(page);
	}
	if (UNEXPECTED(block == 0)) {
		if (atomic_load_explicit(&page->thread_free, memory_order_acquire) != 0) {
			page_adopt_thread_free_block_list(page);
			block = (page->local_free != 0) ? page_get_local_free_block(page) : 0;
		}
		if (block == 0) {
			if (page->block_initialized == page->block_count) {
				rpmalloc_assert(page->block_used == page->block_count, "Page block use counter out of sync");
				rpmalloc_assert(!page->is_full, "Page block use counter out of sync with full flag");
				return 0;
			}
			block = page_initialize_blocks(page);
#if ENABLE_DECOMMIT
			// Committing the memory of the block failed
			if (UNEXPECTED(block == 0))
				return 0;
#endif
			is_zero = page->is_zero;
		}
	}

	rpmalloc_assert(page->block_used <= page->block_count, "Page block use counter out of sync");
	if (page->local_free && !page->heap->local_free[page->size_class])
		page_push_local_free_to_heap(page);

	if (zero_size) {
		if (!is_zero)
			memset(block, 0, zero_size);
		else
			*(uintptr_t*)block = 0;
	}

	return block;
}

////////////
///
/// Span interface
///
//////

static inline int
span_is_thread_heap(span_t* span) {
#if RPMALLOC_FIRST_CLASS_HEAPS
	return ((span->heap == get_thread_heap()) || !span->heap->owner_thread);
#else
	return (span->heap == get_thread_heap());
#endif
}

#if ENABLE_DECOMMIT
//! Commit the span metadata up to the given offset, for spans committing on demand
static int
span_commit_metadata(span_t* span, size_t metadata_end) {
	if (metadata_end <= span->metadata_commit)
		return 0;
	size_t commit_end = (metadata_end + (SPAN_METADATA_COMMIT_CHUNK - 1)) & ~(size_t)(SPAN_METADATA_COMMIT_CHUNK - 1);
	size_t commit_size = commit_end - span->metadata_commit;
	if (global_memory_interface->memory_commit(pointer_offset(span, span->metadata_commit), commit_size) != 0)
		return 1;
#if RPMALLOC_HEAP_STATISTICS
	span->heap->stats.committed_size += commit_size;
#endif
	span->metadata_commit = (uint32_t)commit_end;
	return 0;
}
#endif

//! Find or allocate a page from the given span
static inline page_t*
span_allocate_page(span_t* span) {
	// Allocate path, initialize a new chunk of memory for a page in the given span
	rpmalloc_assert(span->page_initialized < span->page_count, "Page initialization internal failure");
	heap_t* heap = span->heap;
	uint32_t page_index = span->page_initialized;
	page_t* page = span_get_page(span, page_index);
	void* page_memory = pointer_offset(span, (size_t)span->page_size * page_index);
	if (UNEXPECTED(page_index == global_thp_small_page) && (span->page_type == PAGE_SMALL))
		thp_advise_small_span(span);

	// Reserve-only spans commit the page structure here and the page memory in parts as its blocks are
	// initialized; fully committed spans are already committed from the map and must not be committed per page
#if ENABLE_DECOMMIT
	if (span->commit_on_demand &&
	    (span_commit_metadata(span, (size_t)pointer_diff(page, span) + PAGE_HEADER_SIZE) != 0))
		return 0;
#endif
	page->commit_size = span->commit_on_demand ? 0 : span->page_size;
	++span->page_initialized;

	page->page_type = (uint16_t)span->page_type;
	page->commit_on_demand = span->commit_on_demand;
	page->commit_shift = span->commit_shift;
	page->is_zero = 1;
	page->is_decommitted = 0;
	page->heap = heap;
	page->block_start = page_memory;
	rpmalloc_assert(page_is_thread_heap(page), "Page owner thread mismatch");

	// Map the slots of the page to the page structure
	size_t slot_count = (size_t)span->page_size >> SPAN_SLOT_SHIFT;
	page_slot_t* slot = block_get_slot(page_memory);
	for (size_t islot = 0; islot < slot_count; ++islot)
		slot[islot].page = page;

	if (span->page_initialized == span->page_count) {
		// Span fully utilized
		rpmalloc_assert(span == heap->span_partial[span->page_type], "Span partial tracking out of sync");
		heap->span_partial[span->page_type] = 0;

		span->next = heap->span_used[span->page_type];
		heap->span_used[span->page_type] = span;
	}

	return page;
}

//! Cache recently freed huge block mappings for reuse instead of paying a munmap + mmap + page fault round trip
//! per huge allocation cycle. The cache is bounded by demand: cached plus live huge bytes stay within the peak
//! of the requested bytes of live huge blocks over the current and the previous epoch, plus a headroom of
//! HUGE_CACHE_HEADROOM_PERCENT of the peak and at least HUGE_CACHE_HEADROOM_MIN bytes, and the cached bytes
//! alone stay within HUGE_CACHE_CACHED_LIMIT whatever the demand. Mappings idle for an epoch are unmapped, then
//! the least recently freed ones while over either bound, the first one holding more than the excess trimmed by
//! it instead. Idle mappings are unmapped by the huge block paths and by the page refill, page free overflow,
//! thread exit and collect paths, so a program that keeps allocating releases them without a further huge block
//! allocation or free. A mapping that can decommit is reused for a request within its address range and of at
//! least two fifths of its committed size: it is committed up to the request and keeps at most the request size
//! again as committed slack, within the room left under the budget, the tail beyond is decommitted. The span page
//! count holds the committed pages, the page request the usable size and the mapped size the address range.
#ifndef HUGE_CACHE_SLOT_COUNT
#define HUGE_CACHE_SLOT_COUNT 32
#endif
#ifndef HUGE_CACHE_HEADROOM_PERCENT
#define HUGE_CACHE_HEADROOM_PERCENT 20
#endif
#ifndef HUGE_CACHE_HEADROOM_MIN
#define HUGE_CACHE_HEADROOM_MIN (64 * 1024 * 1024)
#endif
//! Largest number of committed bytes the cache holds, bounding what a burst of large blocks leaves cached
#ifndef HUGE_CACHE_CACHED_LIMIT
#define HUGE_CACHE_CACHED_LIMIT (1024 * 1024 * 1024)
#endif
#ifndef HUGE_CACHE_EPOCH_MS
#define HUGE_CACHE_EPOCH_MS 1000
#endif

//! Committed bytes of a huge block mapping
static inline size_t
span_huge_committed(span_t* span) {
	return (size_t)span->page_count * span->page_size;
}

#if ENABLE_STATISTICS
static void
statistics_add_huge_alloc(size_t size) {
	size_t huge_alloc_current =
	    atomic_fetch_add_explicit(&global_statistics.huge_alloc, size, memory_order_relaxed) + size;
	size_t huge_alloc_peak = atomic_load_explicit(&global_statistics.huge_alloc_peak, memory_order_relaxed);
	while (huge_alloc_current > huge_alloc_peak) {
		if (atomic_compare_exchange_weak_explicit(&global_statistics.huge_alloc_peak, &huge_alloc_peak,
		                                          huge_alloc_current, memory_order_relaxed, memory_order_relaxed))
			break;
	}
}
#endif

#if HUGE_CACHE_SLOT_COUNT

//! A cached mapping, with what the lookup compares kept in the slot instead of the span header
typedef struct huge_cache_slot_t {
	//! Cached mapping
	span_t* span;
	//! Committed bytes of the mapping
	size_t committed;
	//! Address range the mapping can commit
	size_t range;
	//! Time the mapping was freed
	uint64_t free_ms;
	//! Nonzero if the mapping can commit and decommit its tail
	uint32_t decommit;
} huge_cache_slot_t;

typedef struct huge_cache_t {
	//! Cache lock
	atomic_uintptr_t lock;
	//! Committed bytes of cached mappings
	size_t cached;
	//! Committed bytes of live huge blocks, including slack kept from reused mappings
	size_t live;
	//! Requested bytes of live huge blocks
	size_t live_request;
	//! Peak of the requested bytes of live huge blocks in the current and the previous epoch
	size_t peak[2];
	//! Start of the current epoch
	uint64_t epoch_ms;
	//! Time the least recently freed cached mapping has been idle for an epoch, zero when the cache is empty
	atomic_ullong idle_ms;
	//! Number of cached mappings
	uint32_t count;
	//! Cached mappings, least recently freed first
	huge_cache_slot_t slot[HUGE_CACHE_SLOT_COUNT];
} huge_cache_t;

static huge_cache_t global_huge_cache;

//! A cached mapping taken out of the cache to decommit the committed bytes beyond the given size outside the lock
typedef struct huge_cache_trim_t {
	span_t* span;
	size_t committed;
	uint64_t free_ms;
} huge_cache_trim_t;

static inline void
huge_cache_lock_acquire(void) {
	uintptr_t lock = 0;
	uint32_t spin = 0;
	while (!atomic_compare_exchange_weak_explicit(&global_huge_cache.lock, &lock, 1, memory_order_acquire,
	                                              memory_order_relaxed)) {
		lock = 0;
		wait_spin(&spin);
	}
}

static inline void
huge_cache_lock_release(void) {
	atomic_store_explicit(&global_huge_cache.lock, 0, memory_order_release);
}

static inline size_t
huge_cache_sub(size_t value, size_t sub) {
	return (value > sub) ? (value - sub) : 0;
}

//! Start a new epoch once the current one has passed, keeping the peak of the previous one, and record the
//! requested bytes of live blocks in the peak (lock held)
static void
huge_cache_epoch(uint64_t now) {
	if (now >= (global_huge_cache.epoch_ms + HUGE_CACHE_EPOCH_MS)) {
		int previous_ended = (now >= (global_huge_cache.epoch_ms + (2 * HUGE_CACHE_EPOCH_MS)));
		global_huge_cache.peak[1] = previous_ended ? global_huge_cache.live_request : global_huge_cache.peak[0];
		global_huge_cache.peak[0] = global_huge_cache.live_request;
		global_huge_cache.epoch_ms = now;
	}
	if (global_huge_cache.live_request > global_huge_cache.peak[0])
		global_huge_cache.peak[0] = global_huge_cache.live_request;
}

//! Bytes that cached and live huge blocks may use (lock held)
static inline size_t
huge_cache_budget(void) {
	size_t peak = global_huge_cache.peak[0];
	if (global_huge_cache.peak[1] > peak)
		peak = global_huge_cache.peak[1];
	size_t headroom = (peak / 100) * HUGE_CACHE_HEADROOM_PERCENT;
	if (headroom < (size_t)HUGE_CACHE_HEADROOM_MIN)
		headroom = (size_t)HUGE_CACHE_HEADROOM_MIN;
	return peak + headroom;
}

//! Remove the slot from the cache, keeping the order of the others (lock held)
static void
huge_cache_remove(uint32_t islot) {
	huge_cache_slot_t* slot = global_huge_cache.slot;
	global_huge_cache.cached -= slot[islot].committed;
	--global_huge_cache.count;
	memmove(slot + islot, slot + islot + 1, (global_huge_cache.count - islot) * sizeof(huge_cache_slot_t));
}

//! Remove the mappings idle for an epoch, then the least recently freed ones while cached and live bytes plus the
//! given extra bytes are over the given budget or cached bytes plus the extra bytes are over the cached limit. The
//! removed mappings are chained to the purge list through the span next pointer, to be unmapped outside the lock.
//! The mappings are ordered by the time they were freed, so both are the first ones. A mapping over the bounds that
//! can decommit and holds more than the excess is taken out to be trimmed by it outside the lock instead, as
//! unmapping the whole mapping would leave the budget unused by that much and the next request reusing a mapping
//! would fault in the pages again (lock held)
static span_t*
huge_cache_evict(uint64_t now, size_t extra, size_t budget, span_t* purge, huge_cache_trim_t* trim) {
	huge_cache_slot_t* slot = global_huge_cache.slot;
	uint32_t count = global_huge_cache.count;
	uint32_t evict = 0;
	while (evict < count) {
		size_t used = global_huge_cache.live + global_huge_cache.cached + extra;
		size_t cached = global_huge_cache.cached + extra;
		int idle = (now >= (slot[evict].free_ms + HUGE_CACHE_EPOCH_MS));
		if (!idle && (used <= budget) && (cached <= (size_t)HUGE_CACHE_CACHED_LIMIT))
			break;
		global_huge_cache.cached -= slot[evict].committed;
		if (!idle && !trim->span && slot[evict].decommit) {
			size_t over = (used > budget) ? (used - budget) : 0;
			if ((cached > (size_t)HUGE_CACHE_CACHED_LIMIT) && ((cached - (size_t)HUGE_CACHE_CACHED_LIMIT) > over))
				over = cached - (size_t)HUGE_CACHE_CACHED_LIMIT;
			// The tail is decommitted from a huge page boundary in transparent huge page mode
			size_t granularity = (global_thp_mask & (1U << PAGE_HUGE)) ? os_thp_size : slot[evict].span->page_size;
			size_t committed = slot[evict].committed;
			if (committed > (over + granularity)) {
				size_t keep = committed - over;
				trim->span = slot[evict].span;
				trim->committed = keep - (keep % granularity);
				trim->free_ms = slot[evict].free_ms;
				++evict;
				break;
			}
		}
		slot[evict].span->next = purge;
		purge = slot[evict].span;
		++evict;
	}
	if (evict) {
		count -= evict;
		memmove(slot, slot + evict, count * sizeof(huge_cache_slot_t));
		global_huge_cache.count = count;
	}
	uint64_t idle_ms = count ? (slot[0].free_ms + HUGE_CACHE_EPOCH_MS) : 0;
	atomic_store_explicit(&global_huge_cache.idle_ms, idle_ms, memory_order_relaxed);
	return purge;
}

static void
huge_cache_unmap(span_t* purge) {
	while (purge) {
		span_t* next = purge->next;
		global_memory_interface->memory_unmap(purge, purge->offset, purge->mapped_size);
		purge = next;
	}
}

//! Decommit the tail of a mapping taken out of the cache to be trimmed, and cache it again at its place in the order
//! of the time it was freed if it still fits the bounds, otherwise unmap it
static void
huge_cache_trim(huge_cache_trim_t* trim) {
	span_t* span = trim->span;
	if (!span)
		return;
	size_t committed = span_huge_committed(span);
	if (global_memory_interface->memory_decommit(pointer_offset(span, trim->committed), committed - trim->committed)) {
		global_memory_interface->memory_unmap(span, span->offset, span->mapped_size);
		return;
	}
	span->page_count = (uint32_t)(trim->committed / span->page_size);
	committed = trim->committed;
	int cached = 0;
	huge_cache_lock_acquire();
	uint32_t count = global_huge_cache.count;
	if ((count < HUGE_CACHE_SLOT_COUNT) &&
	    ((global_huge_cache.live + global_huge_cache.cached + committed) <= huge_cache_budget()) &&
	    ((global_huge_cache.cached + committed) <= (size_t)HUGE_CACHE_CACHED_LIMIT)) {
		huge_cache_slot_t* slot = global_huge_cache.slot;
		uint32_t islot = count;
		while (islot && (slot[islot - 1].free_ms > trim->free_ms))
			--islot;
		memmove(slot + islot + 1, slot + islot, (count - islot) * sizeof(huge_cache_slot_t));
		slot[islot].span = span;
		slot[islot].committed = committed;
		slot[islot].range = (size_t)span->mapped_size - span->offset;
		slot[islot].free_ms = trim->free_ms;
		slot[islot].decommit = 1;
		global_huge_cache.count = count + 1;
		global_huge_cache.cached += committed;
		atomic_store_explicit(&global_huge_cache.idle_ms, slot[0].free_ms + HUGE_CACHE_EPOCH_MS, memory_order_relaxed);
		cached = 1;
	}
	huge_cache_lock_release();
	if (!cached)
		global_memory_interface->memory_unmap(span, span->offset, span->mapped_size);
}

//! Add committed and requested bytes to the live huge blocks, evicting cached mappings over the budget
static void
huge_cache_live_add(size_t committed, size_t request) {
	uint64_t now = monotonic_time_ms();
	huge_cache_lock_acquire();
	global_huge_cache.live += committed;
	global_huge_cache.live_request += request;
	huge_cache_epoch(now);
	huge_cache_trim_t trim = {0};
	span_t* purge = huge_cache_evict(now, 0, huge_cache_budget(), 0, &trim);
	huge_cache_lock_release();
	huge_cache_unmap(purge);
	huge_cache_trim(&trim);
}

//! Remove committed and requested bytes from the live huge blocks
static void
huge_cache_live_sub(size_t committed, size_t request) {
	huge_cache_lock_acquire();
	global_huge_cache.live = huge_cache_sub(global_huge_cache.live, committed);
	global_huge_cache.live_request = huge_cache_sub(global_huge_cache.live_request, request);
	huge_cache_lock_release();
}

//! Unmap cached huge mappings idle for an epoch and the ones over the decayed budget. Called from paths outside
//! the huge block paths, so a program that stops using huge blocks does not keep the cache until its next huge
//! block allocation or free
static NOINLINE void
huge_cache_collect(uint64_t now) {
	uint64_t idle_ms = (uint64_t)atomic_load_explicit(&global_huge_cache.idle_ms, memory_order_relaxed);
	if (!idle_ms || (now < idle_ms))
		return;
	huge_cache_lock_acquire();
	huge_cache_epoch(now);
	huge_cache_trim_t trim = {0};
	span_t* purge = huge_cache_evict(now, 0, huge_cache_budget(), 0, &trim);
	huge_cache_lock_release();
	huge_cache_unmap(purge);
	huge_cache_trim(&trim);
}

//! Unmap cached huge mappings idle for an epoch from the page refill path. The clock is read only while the cache
//! holds mappings, an empty cache costs one relaxed load
static inline void
huge_cache_collect_idle(void) {
	if (atomic_load_explicit(&global_huge_cache.idle_ms, memory_order_relaxed))
		huge_cache_collect(monotonic_time_ms());
}

//! Unmap all cached huge mappings, the cache must not outlive allocator finalization
static void
huge_cache_flush(void) {
	span_t* purge = 0;
	huge_cache_lock_acquire();
	for (uint32_t islot = 0; islot < global_huge_cache.count; ++islot) {
		global_huge_cache.slot[islot].span->next = purge;
		purge = global_huge_cache.slot[islot].span;
	}
	global_huge_cache.count = 0;
	global_huge_cache.cached = 0;
	atomic_store_explicit(&global_huge_cache.idle_ms, 0, memory_order_relaxed);
	global_huge_cache.peak[0] = global_huge_cache.live_request;
	global_huge_cache.peak[1] = global_huge_cache.live_request;
	global_huge_cache.epoch_ms = 0;
	huge_cache_lock_release();
	huge_cache_unmap(purge);
}

//! Remove a freed huge block from the live blocks and try to cache its mapping, returns nonzero if cached
static int
huge_cache_push(span_t* span) {
	size_t committed = span_huge_committed(span);
	int cached = 0;
	uint64_t now = monotonic_time_ms();
	huge_cache_lock_acquire();
	// Start a new epoch before the block leaves the live blocks, it was live up to now and counts in the peak even
	// when no huge block was allocated or freed for epochs
	huge_cache_epoch(now);
	global_huge_cache.live = huge_cache_sub(global_huge_cache.live, committed);
	global_huge_cache.live_request =
	    huge_cache_sub(global_huge_cache.live_request, (size_t)span->page_request * span->page_size);
	// A mapping from before a reinitialization with another page size is not reused, and one larger than the
	// cached limit or the budget left by the live blocks can never be cached, neither makes room by evicting other
	// mappings
	int cacheable = (span->page_size == global_config.page_size) && (committed <= (size_t)HUGE_CACHE_CACHED_LIMIT) &&
	                ((global_huge_cache.live + committed) <= huge_cache_budget());
	huge_cache_trim_t trim = {0};
	span_t* purge = huge_cache_evict(now, cacheable ? committed : 0, huge_cache_budget(), 0, &trim);
	if (cacheable && ((global_huge_cache.live + global_huge_cache.cached + committed) <= huge_cache_budget()) &&
	    ((global_huge_cache.cached + committed) <= (size_t)HUGE_CACHE_CACHED_LIMIT)) {
		huge_cache_slot_t* slot = global_huge_cache.slot;
		if (global_huge_cache.count == HUGE_CACHE_SLOT_COUNT) {
			// All slots in use (and none evicted above), replace the least recently freed mapping
			slot[0].span->next = purge;
			purge = slot[0].span;
			huge_cache_remove(0);
		}
		uint32_t count = global_huge_cache.count;
		// The clock was read before taking the lock, keep the slots ordered by the time they were freed
		uint64_t free_ms = now;
		if (count && (slot[count - 1].free_ms > free_ms))
			free_ms = slot[count - 1].free_ms;
		slot[count].span = span;
		slot[count].committed = committed;
		slot[count].range = (size_t)span->mapped_size - span->offset;
		slot[count].free_ms = free_ms;
		slot[count].decommit = span->commit_on_demand;
		global_huge_cache.count = count + 1;
		global_huge_cache.cached += committed;
		atomic_store_explicit(&global_huge_cache.idle_ms, slot[0].free_ms + HUGE_CACHE_EPOCH_MS, memory_order_relaxed);
		cached = 1;
	}
	huge_cache_lock_release();
	huge_cache_unmap(purge);
	huge_cache_trim(&trim);
	return cached;
}

//! Add a huge block allocation of the given (page aligned) size to the live blocks and take the cached mapping
//! that fits it best, returns 0 on a miss. A mapping that can decommit fits a request within its address range
//! and of at least two fifths of its committed size, the best one needs the fewest bytes committed up to the
//! request or decommitted down to twice the request, the most recently freed of those. A larger mapping is left
//! for a larger request: reusing it decommits more than half the request beyond twice the request, pages a larger
//! request would reuse, and with the cache full of large mappings the smaller requests of a program decommitted
//! their pages for the larger requests to fault in again. A reused mapping keeps committed pages beyond the
//! request, up to twice the request, only within the room left under the budget. Other mappings are reused only
//! for requests of at least half their committed size and keep all their committed pages. The committed size the
//! block is accounted with, the request size on a miss, is returned in commit_size. A request missing the cache
//! while it holds mappings too large for it takes the committed bytes it maps out of the cache, trimming or
//! unmapping the least recently freed mappings by them, as a program allocating smaller blocks after freeing
//! larger ones would otherwise keep the larger mappings cached on top of the smaller blocks until they are idle.
static span_t*
huge_cache_pop(size_t alloc_size, size_t* commit_size) {
	size_t slack_limit = (alloc_size <= (SIZE_MAX / 2)) ? (alloc_size * 2) : SIZE_MAX;
	size_t reuse_limit = (alloc_size <= (SIZE_MAX / 5)) ? ((alloc_size * 5) / 2) : SIZE_MAX;
	span_t* best = 0;
	size_t best_cost = 0;
	size_t best_committed = 0;
	uint32_t best_decommit = 0;
	uint64_t now = monotonic_time_ms();
	huge_cache_lock_acquire();
	global_huge_cache.live_request += alloc_size;
	huge_cache_epoch(now);
	size_t used = global_huge_cache.live + global_huge_cache.cached;
	uint32_t count = global_huge_cache.count;
	uint32_t best_slot = count;
	int too_large = 0;
	for (uint32_t islot = 0; islot < count; ++islot) {
		const huge_cache_slot_t* slot = global_huge_cache.slot + islot;
		size_t committed = slot->committed;
		size_t cost = 0;
		if (slot->decommit) {
			if (slot->range < alloc_size)
				continue;
			if (committed > reuse_limit) {
				too_large = 1;
				continue;
			}
			if (committed < alloc_size)
				cost = alloc_size - committed;
			else if (committed > slack_limit)
				cost = committed - slack_limit;
		} else if ((committed < alloc_size) || (committed > slack_limit)) {
			continue;
		}
		if ((best_slot == count) || (cost < best_cost) || ((cost == best_cost) && (committed <= best_committed))) {
			best_slot = islot;
			best_cost = cost;
			best_committed = committed;
		}
	}
	size_t commit = alloc_size;
	size_t spare = 0;
	if (best_slot < count) {
		best = global_huge_cache.slot[best_slot].span;
		best_decommit = global_huge_cache.slot[best_slot].decommit;
		huge_cache_remove(best_slot);
		if (best_committed > alloc_size)
			spare = ((best_committed > slack_limit) ? slack_limit : best_committed) - alloc_size;
		// A mapping that cannot decommit keeps all its committed pages
		if (!best_decommit) {
			commit += spare;
			spare = 0;
		}
	}
	global_huge_cache.live += commit;
	size_t budget = huge_cache_budget();
	if (!best && too_large && (budget > used))
		budget = used;
	huge_cache_trim_t trim = {0};
	span_t* purge = huge_cache_evict(now, 0, budget, 0, &trim);
	if (spare) {
		// Keep the spare committed pages of the reused mapping only within the room left under the budget, the
		// cached mappings save more faults than the spare pages. The rest of the tail is decommitted, from a huge
		// page boundary in transparent huge page mode
		used = global_huge_cache.live + global_huge_cache.cached;
		size_t room = (budget > used) ? (budget - used) : 0;
		size_t granularity = (global_thp_mask & (1U << PAGE_HUGE)) ? os_thp_size : best->page_size;
		if (spare > room) {
			size_t end = alloc_size + room;
			end -= end % granularity;
			spare = (end > alloc_size) ? (end - alloc_size) : 0;
		}
		commit += spare;
		global_huge_cache.live += spare;
	}
	huge_cache_lock_release();
	huge_cache_unmap(purge);
	huge_cache_trim(&trim);
	*commit_size = commit;
	return best;
}

#endif

//! Add a huge block span to the huge span list of its heap, which rpmalloc_heap_free_all releases. Only heaps
//  without an owner thread keep the list, as the huge blocks of a thread heap are freed by any thread. The list
//  links forward through the span next pointer and back through the prev pointer of the huge block page, which
//  is in no page list, so freeing the block removes the span without walking the list
static void
span_huge_track(span_t* span) {
	heap_t* heap = span->heap;
	page_t* page = span_get_huge_page(span);
	span_t* head = heap->span_used[PAGE_HUGE];
	span->next = head;
	page->prev = 0;
	if (head)
		span_get_huge_page(head)->prev = page;
	heap->span_used[PAGE_HUGE] = span;
	page->is_tracked = 1;
}

//! Remove a huge block span from the huge span list of its heap
static void
span_huge_untrack(span_t* span) {
	heap_t* heap = span->heap;
	page_t* page = span_get_huge_page(span);
	page_t* prev = page->prev;
	span_t* next = span->next;
	if (prev)
		page_get_span(prev)->next = next;
	else
		heap->span_used[PAGE_HUGE] = next;
	if (next)
		span_get_huge_page(next)->prev = prev;
	page->is_tracked = 0;
}

//! Free a huge block by caching or unmapping its mapping. Kept out of line so the generic free path,
//  taken by every cross-thread free, needs no stack frame
static NOINLINE void
span_deallocate_huge(span_t* span) {
#if ENABLE_STATISTICS
	statistics_sub_saturating(&global_statistics.huge_alloc, span_huge_committed(span));
#endif
#if RPMALLOC_HEAP_STATISTICS
	if (span->heap) {
		span->heap->stats.mapped_size -= span->mapped_size;
#if ENABLE_DECOMMIT
		span->heap->stats.committed_size -= span_huge_committed(span);
#else
		span->heap->stats.committed_size -= span->mapped_size;
#endif
	}
#endif
	// Leave the heap span list before the mapping is cached for any heap to reuse or is unmapped, the list must
	// only hold live blocks for rpmalloc_heap_free_all to release
	if (span_get_huge_page(span)->is_tracked)
		span_huge_untrack(span);
#if HUGE_CACHE_SLOT_COUNT
	if (huge_cache_push(span))
		return;
#endif
	global_memory_interface->memory_unmap(span, span->offset, span->mapped_size);
}

//! Free a block in a page holding aligned blocks, which first finds the block start. Kept out of line
//  like the huge block free
static NOINLINE void
span_deallocate_aligned_block(page_t* page, void* block) {
	block = page_block_realign(page, block);
	if (page_is_thread_heap(page))
		page_put_local_free_block(page, block);
	else
		page_put_thread_free_block(page, block);
}

static NOINLINE void
span_deallocate_block(page_t* page, void* block) {
	if (UNEXPECTED(page->page_type == PAGE_HUGE)) {
		span_deallocate_huge(page_get_span(page));
		return;
	}

	if (UNEXPECTED(page->has_aligned_block)) {
		span_deallocate_aligned_block(page, block);
		return;
	}

	int is_thread_local = page_is_thread_heap(page);
	if (EXPECTED(is_thread_local != 0)) {
		page_put_local_free_block(page, block);
	} else {
		// Multithreaded deallocation. A free into the same page as the previous one is linked into the
		// thread free chain, otherwise the chain is pushed to its page
		thread_context_t* thread = &global_thread;
		if (EXPECTED(thread->chain_page == page)) {
			uint32_t chain_count = thread->chain_count;
			if (UNEXPECTED(!chain_count)) {
				thread_free_chain_begin(page, block);
				return;
			}
			rpmalloc_assert(!(page_block_offset(page, block) % page->block_size),
			                "Block pointer is not aligned to start of block");
			rpmalloc_assert(page->page_type <= PAGE_MEDIUM_SMALL, "Chained free into a page type not batched");
#if RPMALLOC_FIRST_CLASS_HEAPS
			rpmalloc_assert(!page->heap_first_class, "Chained free into a page of a first class heap");
#endif
			*thread->chain_link = block;
			thread->chain_link = &((block_t*)block)->next;
			thread->chain_count = ++chain_count;
#if THREAD_FREE_CHAIN_LIMIT_BYTES
			// The block size is on the page header line the checks above read
			if (UNEXPECTED((chain_count * page->block_size) >= THREAD_FREE_CHAIN_LIMIT_BYTES))
				thread_free_chain_flush(thread);
#endif
			return;
		}
		// A free into another page is pushed at once and makes the page the chain page. Only the store is added
		// to the plain push, random cross-thread frees rarely free into the same page twice in a row
		if (UNEXPECTED(thread->chain_count != 0)) {
			thread_free_chain_switch_flush(page, block);
			return;
		}
		thread->chain_page = page;
		page_put_thread_free_block(page, block);
	}
}

////////////
///
/// Block interface
///
//////

static inline span_t*
block_get_span(block_t* block) {
	return (span_t*)((uintptr_t)block & SPAN_MASK);
}

//! Deallocate a block, taking the fast path for a block in a page whose fast free key is the key of the owned
//  heap. The caller owns that heap, it is the first class heap the block was allocated from, or the thread heap
//  if the given heap is null
static inline void
block_deallocate_heap(block_t* block, heap_t* owned_heap) {
	// The key and the page are loaded from the same slot table entry, so the ownership and eligibility check
	// does not wait on a load of the page structure
	page_slot_t* slot = block_get_slot(block);
	page_t* page = slot->page;
	uintptr_t key = atomic_load_explicit(&slot->key, memory_order_relaxed);
#if defined(__GNUC__) || defined(__clang__)
	// Keep the AArch64 machine outliner from moving the compare out of the free fast path into a call
	__asm__("" : "+r"(key));
#endif
	// The thread heap is read after the slot table entry loads are issued, the thread local storage access is
	// a call on some platforms and loads are not moved across it
	if (!owned_heap)
		owned_heap = get_thread_heap();

#if RPMALLOC_HEAP_STATISTICS
	heap_t* heap = page->heap;
	if (heap) {
		if (page->page_type <= PAGE_LARGE) {
			heap->stats.allocated_size -= page->block_size;
		} else {
			span_t* span = block_get_span(block);
			heap->stats.allocated_size -= ((size_t)span->page_size * (size_t)span->page_request);
		}
	}
#endif

	// Optimized path for a free by the heap owner with non-huge block in page that is not full and has
	// no aligned blocks, all of which the key encodes by matching the owned heap
	if (EXPECTED(key == page_key(owned_heap))) {
		block->next = page->local_free;
		page->local_free = block;
		_rpmalloc_stat_add_free(page->heap, page->size_class, 1);
		if (UNEXPECTED(--page->block_used == 0))
			page_available_to_free(page);
	} else {
		span_deallocate_block(page, block);
	}
}

static inline void
block_deallocate(block_t* block) {
	block_deallocate_heap(block, 0);
}

static inline size_t
block_usable_size(block_t* block) {
	page_t* page = block_get_page(block);
	if (EXPECTED(page->page_type <= PAGE_LARGE)) {
		// Offset within a page is below 2^32, a 32-bit modulo is several times cheaper than a 64-bit one
		uint32_t block_offset = page_block_offset(page, block);
		return page->block_size - (block_offset % page->block_size);
	} else {
		span_t* span = block_get_span(block);
		return ((size_t)span->page_size * (size_t)span->page_request) - (size_t)pointer_diff(block, span);
	}
}

////////////
///
/// Heap interface
///
//////

static inline void
heap_lock_acquire(void) {
	uintptr_t lock = 0;
	uintptr_t this_lock = get_thread_id();
	uint32_t spin = 0;
	while (!atomic_compare_exchange_strong(&global_heap_lock, &lock, this_lock)) {
		lock = 0;
		wait_spin(&spin);
	}
}

static inline void
heap_lock_release(void) {
	rpmalloc_assert((uintptr_t)atomic_load_explicit(&global_heap_lock, memory_order_relaxed) == get_thread_id(),
	                "Bad heap lock");
	atomic_store_explicit(&global_heap_lock, 0, memory_order_release);
}

static inline heap_t*
heap_initialize(void* block) {
	heap_t* heap = block;
	memset_const(heap, 0, sizeof(heap_t));
	heap->id = 1 + atomic_fetch_add_explicit(&global_heap_id, 1, memory_order_relaxed);
	return heap;
}

// A heap control structure is much smaller than the mapping page size (and far smaller than a
// huge page), so a single page-aligned mapping is carved into as many heaps as fit, the first
// returned to the caller and the extras queued for reuse. This avoids mapping (and, under huge
// pages, committing) a whole page per heap. Only the first heap records the mapping offset and
// size; it owns the shared mapping and is the one heap that unmaps it at finalize.
//
// Block creation is serialized through global_heap_creating: when no heap is available, one thread
// maps and carves a block while concurrent thread starts wait for the carved heaps to be published,
// so a startup burst maps a single block instead of one block per thread. Ordinary requests reuse a
// released heap first, then a pristine carved one. First-class requests need a heap with no prior
// allocations, so they take only pristine heaps (a carved extra, or a freshly mapped master), never
// a released one, but still wait so they do not map concurrently with another creation.
static heap_t*
heap_allocate_new(int first_class) {
	if (!global_config.page_size)
		rpmalloc_initialize(0);

	heap_lock_acquire();
	uint32_t spin = 0;
	while (1) {
		if (!first_class && global_heap_queue) {
			heap_t* heap = global_heap_queue;
			global_heap_queue = heap->next;
			if (global_heap_orphan_cursor == heap)
				global_heap_orphan_cursor = 0;
			atomic_store_explicit(&heap->orphaned, 0, memory_order_relaxed);
			heap_lock_release();
			if (heap->release_trim) {
				heap->release_trim = 0;
				heap_release_free_pages(heap);
			}
			// A released first class heap becomes a thread heap, the first class heap functions must not be
			// called on it once released, so cross-thread frees into its pages can be chained, into the pages
			// it already has once they are initialized again for a size class
			heap->first_class = 0;
			return heap;
		}
		if (global_heap_pristine) {
			heap_t* heap = global_heap_pristine;
			global_heap_pristine = heap->next;
			heap_lock_release();
			return heap;
		}
		if (!global_heap_creating)
			break;
		heap_lock_release();
		wait_spin(&spin);
		heap_lock_acquire();
	}
	global_heap_creating = 1;
	heap_lock_release();

	size_t aligned_heap_size = HEAP_ALIGNMENT * ((sizeof(heap_t) + (HEAP_ALIGNMENT - 1)) / HEAP_ALIGNMENT);
	size_t block_size = get_page_aligned_size(aligned_heap_size);
	size_t offset = 0;
	size_t mapped_size = 0;
	block_t* block = memory_interface_map(block_size, 0, &offset, &mapped_size);
#if ENABLE_DECOMMIT
	if (block && os_commit_on_demand() && (global_memory_interface->memory_commit(block, block_size) != 0)) {
		global_memory_interface->memory_unmap(block, offset, mapped_size);
		block = 0;
	}
#endif
	if (!block) {
		heap_lock_acquire();
		global_heap_creating = 0;
		heap_lock_release();
		return 0;
	}

	heap_t* heap = heap_initialize((void*)block);
	heap->offset = (uint32_t)offset;
	heap->mapped_size = mapped_size;

	// Carve the remaining heaps into a local chain, then splice it onto the pristine pool in one
	// step so the lock is not held across the per-heap initialization. heap_initialize zeroes each
	// structure, leaving mapped_size == 0 so the extras are not treated as mapping owners. They are
	// pristine until handed out, so first-class requests may reuse them.
	size_t heap_count = mapped_size / aligned_heap_size;
	heap_t* chain_head = 0;
	heap_t* chain_tail = 0;
	heap_t* extra_heap = (heap_t*)pointer_offset(heap, aligned_heap_size);
	for (size_t iheap = 1; iheap < heap_count; ++iheap) {
		heap_initialize((void*)extra_heap);
		if (!chain_tail)
			chain_tail = extra_heap;
		extra_heap->next = chain_head;
		chain_head = extra_heap;
		extra_heap = (heap_t*)pointer_offset(extra_heap, aligned_heap_size);
	}

	heap_lock_acquire();
	if (chain_tail) {
		chain_tail->next = global_heap_pristine;
		global_heap_pristine = chain_head;
	}
	global_heap_creating = 0;
	heap_lock_release();
	return heap;
}

// Unmaps the shared block owned by this heap. INVARIANT: heap blocks are only ever unmapped here,
// at global finalize, never while the allocator is running. Several heaps share a block, and a heap
// may be borrowed by an unrelated owner's thread or by a first-class heap that outlives the thread
// whose request mapped the block (see heap_allocate_new). Unmapping a block mid-run would leave
// those borrowed heaps pointing at freed memory, so any future block reclamation must guarantee no
// heap carved from the block is still live.
static void
heap_unmap(heap_t* heap) {
	global_memory_interface->memory_unmap(heap, heap->offset, heap->mapped_size);
}

static heap_t*
heap_allocate(int first_class) {
	heap_t* heap = heap_allocate_new(first_class);
	if (heap) {
		uintptr_t current_thread_id = get_thread_id();
		heap_lock_acquire();
		heap->next = global_heap_used;
		heap->prev = 0;
		if (global_heap_used)
			global_heap_used->prev = heap;
		global_heap_used = heap;
		heap_lock_release();
		heap->owner_thread = current_thread_id;
#if ENABLE_STATISTICS
		// Counters belong to the heap, not the thread, so alloc_current reflects the blocks live in
		// the heap's pages regardless of which thread owns it.
		atomic_fetch_add_explicit(&global_statistics.heap_count, 1, memory_order_relaxed);
#endif
	}
	return heap;
}

static inline void
heap_release(heap_t* heap) {
	if (heap == global_heap_default)
		return;
#if ENABLE_STATISTICS
	statistics_sub_saturating(&global_statistics.heap_count, 1);
#endif
	heap_lock_acquire();
	if (heap->prev)
		heap->prev->next = heap->next;
	if (heap->next)
		heap->next->prev = heap->prev;
	if (global_heap_used == heap)
		global_heap_used = heap->next;
	uint64_t now = monotonic_time_ms();
	heap->release_ms = now;
	heap->next = global_heap_queue;
	global_heap_queue = heap;
	// A heap left in the queue for the orphan delay is unlikely to be adopted soon, as when a thread pool retires
	// idle workers and starts fewer, its pages go to the orphan lists for the running threads to reuse. The queue
	// is a stack changing only at its top, the heaps below are released earlier the deeper they are, and the pass
	// continues where the previous one stopped
	uint32_t orphan_count = 0;
	page_t* orphan_free = 0;
	heap_t* queued = global_heap_orphan_cursor ? global_heap_orphan_cursor : heap->next;
	while (queued && (orphan_count < HEAP_ORPHAN_BATCH)) {
		if ((now - queued->release_ms) >= heap_orphan_delay_ms()) {
			if (!queued->first_class && !atomic_load_explicit(&queued->orphaned, memory_order_relaxed)) {
				queued->orphaning = 1;
				if (atomic_load_explicit(&queued->page_pending, memory_order_relaxed))
					heap_collect_pending_pages(queued);
				heap_collect_thread_free(queued);
				heap_orphan_pages(queued);
				orphan_free = heap_orphan_free_pages(queued, orphan_free);
				queued->orphaning = 0;
				atomic_store_explicit(&queued->orphaned, 1, memory_order_relaxed);
				++orphan_count;
			}
		} else if (global_heap_orphan_cursor) {
			// Heaps below the cursor were released before it, none is younger than the delay
			break;
		}
		queued = queued->next;
	}
	if (global_heap_orphan_cursor || orphan_count)
		global_heap_orphan_cursor = queued;
	heap_lock_release();
	page_orphan_empty_add(orphan_free);
	page_orphan_sweep(now);
}

//! Count a free page of the heap that is no longer committed and free, decommitted or taken for reuse, lowering the
//  low water mark of the decommit window
static inline void
heap_page_free_commit_dec(heap_t* heap, uint32_t page_type) {
	uint32_t count = --heap->page_free_commit_count[page_type];
	if (count < heap->page_free_low[page_type])
		heap->page_free_low[page_type] = count;
}

static void
heap_page_free_decommit(heap_t* heap, uint32_t page_type, uint32_t page_retain_count) {
	page_t* page = heap->page_free[page_type];
	while (page && page_retain_count) {
		page = page->next;
		--page_retain_count;
	}
	while (page && (page->is_decommitted == 0)) {
		page_decommit_memory_pages(page);
		// Decommit can be refused (decommit disabled, page type not larger than the
		// configured page size, or kernel rejecting decommit of huge page mappings),
		// in which case remaining pages cannot be decommitted either
		if (page->is_decommitted == 0)
			break;
		heap_page_free_commit_dec(heap, page_type);
		page = page->next;
	}
}

//! Decommit the most recently freed page at the head of the free list and move it to the tail, behind the
//  committed pages, so the list keeps its committed pages first. Returns zero if no page was decommitted
static int
heap_page_free_decommit_head(heap_t* heap, uint32_t page_type) {
	page_t* page = heap->page_free[page_type];
	if (!page || page->is_decommitted)
		return 0;
	page_decommit_memory_pages(page);
	if (!page->is_decommitted)
		return 0;
	heap_page_free_commit_dec(heap, page_type);
	if (page->next) {
		heap->page_free[page_type] = page->next;
		heap->page_free_tail[page_type]->next = page;
		page->next = 0;
		heap->page_free_tail[page_type] = page;
	}
	return 1;
}

//! Number of processors the process can run on
static uint32_t global_cpu_count;

//! Current decommit epoch, the monotonic time in units of the decommit delay
static atomic_uint global_heap_active_epoch;

//! Number of heaps freeing pages in the current and the previous decommit epoch, by epoch parity
static atomic_uint global_heap_active_count[2];

//! Decommit delay of free pages in milliseconds, shortened while more heaps free pages than processors run them
static atomic_uint global_page_decommit_delay = PAGE_DECOMMIT_DELAY_MS;

//! Count the heap as freeing pages in the current decommit epoch and return the decommit delay. When the epoch moves
//  on, the number of heaps that freed pages in the ended epoch sets the delay: with more of them than processors the
//  threads wait for a processor in turn, and the free pages a waiting heap keeps for its next turn sit unused in
//  every waiting heap. The delay is then scaled down by the ratio, to at least a quarter, so the pages a heap leaves
//  unused through the shorter window are decommitted while it waits. Heaps that do not free pages, of idle or blocked
//  threads, are not counted
static uint32_t
heap_page_decommit_delay(heap_t* heap, uint64_t now) {
	uint32_t epoch = (uint32_t)(now / PAGE_DECOMMIT_DELAY_MS);
	uint32_t current = atomic_load_explicit(&global_heap_active_epoch, memory_order_relaxed);
	if ((epoch != current) && atomic_compare_exchange_strong_explicit(&global_heap_active_epoch, &current, epoch,
	                                                                  memory_order_relaxed, memory_order_relaxed)) {
		uint32_t active = 0;
		if (epoch == current + 1)
			active = atomic_load_explicit(&global_heap_active_count[current & 1], memory_order_relaxed);
		atomic_store_explicit(&global_heap_active_count[epoch & 1], 0, memory_order_relaxed);
		uint32_t delay = PAGE_DECOMMIT_DELAY_MS;
		if (global_cpu_count && (active > global_cpu_count)) {
			delay = (uint32_t)(((uint64_t)PAGE_DECOMMIT_DELAY_MS * global_cpu_count) / active);
			if (delay < (PAGE_DECOMMIT_DELAY_MS / 4))
				delay = PAGE_DECOMMIT_DELAY_MS / 4;
		}
		atomic_store_explicit(&global_page_decommit_delay, delay, memory_order_relaxed);
	}
	if (heap->page_free_epoch != epoch) {
		heap->page_free_epoch = epoch;
		atomic_fetch_add_explicit(&global_heap_active_count[epoch & 1], 1, memory_order_relaxed);
	}
	return atomic_load_explicit(&global_page_decommit_delay, memory_order_relaxed);
}

//! End the decommit window of a page type. The free pages the heap held committed through the whole window, the
//  low water mark of the free committed page count, were not reused and are decommitted down to the retain count,
//  the oldest first. The pages the heap reused during the window stay committed. A new window starts while the heap
//  holds more than the retain count
static void
heap_page_free_window_end(heap_t* heap, uint32_t page_type, uint64_t now) {
	uint32_t retain = global_page_free_retain[page_type];
	uint32_t low = heap->page_free_low[page_type];
	if (low > retain)
		heap_page_free_decommit(heap, page_type, heap->page_free_commit_count[page_type] - (low - retain));
	uint32_t count = heap->page_free_commit_count[page_type];
	heap->page_free_overflow_ms[page_type] = (count > retain) ? (now ? now : 1) : 0;
	heap->page_free_low[page_type] = count;
}

//! Called when a page is freed with the free committed page count of its type above the retain count. The first
//  call starts a decommit window, which tracks the lowest count until it ends after the decommit delay and the pages
//  unused through the whole window are decommitted. A heap reusing its free pages keeps them committed whatever its
//  count, and a heap freeing pages faster than it reuses them, as a heap of a thread whose blocks other threads
//  free, decommits them after a window whatever the number of heaps. Within the window a page freed with the count
//  at the hard limit is decommitted right away, so the limit bounds the committed free pages of a burst
static NOINLINE void
heap_page_free_overflow(heap_t* heap, uint32_t page_type) {
	if (heap->orphaning)
		return;
	uint64_t limit = (uint64_t)global_page_free_overflow[page_type] * PAGE_DECOMMIT_LIMIT_FACTOR;
	uint64_t now = monotonic_time_ms();
#if HUGE_CACHE_SLOT_COUNT
	huge_cache_collect(now);
#endif
	uint64_t since = heap->page_free_overflow_ms[page_type];
	uint32_t delay = heap_page_decommit_delay(heap, now);
	if (!since) {
		heap->page_free_overflow_ms[page_type] = now ? now : 1;
		heap->page_free_low[page_type] = heap->page_free_commit_count[page_type];
	} else if ((now - since) >= delay) {
		heap_page_free_window_end(heap, page_type, now);
		return;
	}
	if (heap->page_free_commit_count[page_type] >= limit)
		heap_page_free_decommit_head(heap, page_type);
}

//! End the decommit windows of other page types than the given one that have run for the decommit delay.
//  The window of a page type was only checked when a page of that type was freed, so the free pages of a
//  type the program stopped freeing stayed committed indefinitely. Page frees and commits of any type
//  check the windows of the other types too
static NOINLINE void
heap_page_free_overflow_other(heap_t* heap, uint32_t page_type) {
	if (heap->orphaning)
		return;
	uint64_t now = monotonic_time_ms();
	uint32_t delay = heap_page_decommit_delay(heap, now);
	for (uint32_t itype = 0; itype < 4; ++itype) {
		uint64_t since = heap->page_free_overflow_ms[itype];
		if ((itype == page_type) || !since || ((now - since) < delay))
			continue;
		heap_page_free_window_end(heap, itype, now);
	}
}

//! Called when the heap takes a new page of the given page type from a span. Free pages of the other page
//  types above their overflow threshold cannot be reused for the new page, so instead of growing the
//  committed memory of the heap by the page, free pages of other types of at least the same size are
//  decommitted. Recommitting a free page of the heap does not, a heap cycling between page types would
//  otherwise decommit and page fault the pages of each type every cycle.
//  The size decommitted beyond the page size is kept as credit for the next commits, so a heap moving
//  from large to small pages decommits one large page per large page worth of small pages
static NOINLINE void
heap_page_commit_trim(heap_t* heap, uint32_t page_type) {
	uint32_t needed = global_page_units[page_type];
	if (heap->page_commit_credit >= needed) {
		heap->page_commit_credit -= needed;
		return;
	}
	needed -= heap->page_commit_credit;
	heap->page_commit_credit = 0;
	for (int itype = PAGE_LARGE; itype >= PAGE_SMALL; --itype) {
		if (((uint32_t)itype == page_type) || (global_page_free_overflow[itype] == 0xFFFFFFFFU))
			continue;
		while (heap->page_free_commit_count[itype] >= global_page_free_overflow[itype]) {
			if (!heap_page_free_decommit_head(heap, (uint32_t)itype))
				break;
			uint32_t units = global_page_units[itype];
			if (units >= needed) {
				heap->page_commit_credit = units - needed;
				return;
			}
			needed -= units;
		}
	}
}

//! Check if free pages of another page type than the given one are held above their overflow threshold
static inline int
heap_page_commit_trim_pending(heap_t* heap, uint32_t page_type) {
	for (uint32_t itype = 0; itype < 4; ++itype) {
		if ((itype != page_type) && (heap->page_free_commit_count[itype] >= global_page_free_overflow[itype]))
			return 1;
	}
	return heap->page_commit_credit != 0;
}

////////////
///
/// Orphan pages
///
//////

//! A thread heap released by its exiting thread keeps its pages until another thread adopts the heap. With fewer
//  threads than released heaps, as when a thread pool retires idle workers and starts others later, the free blocks
//  in the partly used pages of the queued heaps were never reused, while the running threads took new pages. A
//  released thread heap instead hands its available pages to per size class orphan lists, and a thread heap takes
//  an orphan page of the size class before it takes a new page. An orphan page has the queued flag set in its
//  thread free token, so threads freeing into it neither push it to a pending list nor read its heap, and it is
//  owned by the orphan heap, which no thread owns, so every free into it takes the cross-thread path. Orphan pages
//  of each size class, linked through the page next pointer
static atomic_uintptr_t global_page_orphan[SIZE_CLASS_COUNT];
//! Lock of the orphan page lists
static atomic_uintptr_t global_page_orphan_lock;

static void
page_orphan_lock_acquire(void) {
	uintptr_t lock = 0;
	uint32_t spin = 0;
	while (!atomic_compare_exchange_weak_explicit(&global_page_orphan_lock, &lock, 1, memory_order_acquire,
	                                              memory_order_relaxed)) {
		lock = 0;
		wait_spin(&spin);
	}
}

static void
page_orphan_lock_release(void) {
	atomic_store_explicit(&global_page_orphan_lock, 0, memory_order_release);
}

//! Empty orphan pages of each page type, the free pages of orphaned heaps and the orphan pages the sweep emptied,
//  decommitted, linked through the page next pointer
static atomic_uintptr_t global_page_orphan_empty[4];
//! Time (monotonic ms) of the last sweep of the orphan lists
static atomic_ullong global_page_orphan_sweep_ms;

//! Move the orphan pages whose blocks have all been freed to the empty orphan pages of their page type and decommit
//  them, at most once per orphan delay. Blocks are freed into orphan pages by other threads after the pages were
//  orphaned, and a page of a size class no running thread takes pages of would otherwise stay resident. An empty
//  orphan page is taken for a page of any size class of its page type
static void
page_orphan_sweep(uint64_t now) {
	// A reap takes the time once for all the heaps it reaps, a sweep by another thread meanwhile can be later
	unsigned long long last = atomic_load_explicit(&global_page_orphan_sweep_ms, memory_order_relaxed);
	if ((now < last) || ((now - last) < heap_orphan_delay_ms()) ||
	    !atomic_compare_exchange_strong_explicit(&global_page_orphan_sweep_ms, &last, now, memory_order_relaxed,
	                                             memory_order_relaxed))
		return;
	page_t* empty = 0;
	page_orphan_lock_acquire();
	for (uint32_t iclass = 0; iclass < SIZE_CLASS_COUNT; ++iclass) {
		page_t* page = (page_t*)atomic_load_explicit(&global_page_orphan[iclass], memory_order_relaxed);
		page_t* prev = 0;
		while (page) {
			page_t* next = page->next;
			unsigned long long token = atomic_load_explicit(&page->thread_free, memory_order_acquire);
			if ((uint32_t)((token >> 32ULL) & THREAD_FREE_COUNT_MASK) == page->block_used) {
				if (prev)
					prev->next = next;
				else
					atomic_store_explicit(&global_page_orphan[iclass], (uintptr_t)next, memory_order_relaxed);
				_rpmalloc_stat_add_free(&global_heap_orphan, iclass, page->block_used);
				page->next = empty;
				empty = page;
			} else {
				prev = page;
			}
			page = next;
		}
	}
	page_orphan_lock_release();
	for (page_t* page = empty; page; page = page->next) {
		// No thread holds a block of the page, every used block is in its thread free list
		atomic_store_explicit(&page->thread_free, 0, memory_order_relaxed);
		page->block_used = 0;
		page->block_initialized = 0;
		page->local_free = 0;
		page->is_full = 0;
		page->is_free = 1;
		page->is_zero = 0;
	}
	page_orphan_empty_add(empty);
}

//! Decommit a chain of free pages, linked through the page next pointer, and add them to the empty orphan pages of
//  their page type. Called without the heap lock, a decommit is a system call
static void
page_orphan_empty_add(page_t* page) {
	while (page) {
		page_t* next = page->next;
		page_decommit_memory_pages(page);
		page_orphan_lock_acquire();
		page->next = (page_t*)atomic_load_explicit(&global_page_orphan_empty[page->page_type], memory_order_relaxed);
		atomic_store_explicit(&global_page_orphan_empty[page->page_type], (uintptr_t)page, memory_order_relaxed);
		page_orphan_lock_release();
		page = next;
	}
}

//! Take the free pages of a released heap the orphan pass hands to the orphan lists, chained in front of the given
//  pages. The decommit window of a heap only runs while its thread frees pages, a heap left in the queue for the
//  orphan delay would otherwise keep its free pages committed until a thread adopts it
static page_t*
heap_orphan_free_pages(heap_t* heap, page_t* chain) {
	for (uint32_t itype = 0; itype < 4; ++itype) {
		page_t* page = heap->page_free[itype];
		while (page) {
			page_t* next = page->next;
#if RPMALLOC_HEAP_STATISTICS && ENABLE_DECOMMIT
			if (!page->is_decommitted)
				heap->stats.committed_size -= page->commit_size;
#endif
			page->heap = &global_heap_orphan;
			page->next = chain;
			chain = page;
			page = next;
		}
		heap->page_free[itype] = 0;
		heap->page_free_tail[itype] = 0;
		heap->page_free_commit_count[itype] = 0;
		heap->page_free_low[itype] = 0;
		heap->page_free_overflow_ms[itype] = 0;
	}
	return chain;
}

//! Take an empty orphan page of the page type as a page of the heap for the size class, returns 0 if there is none
static NOINLINE page_t*
heap_take_orphan_empty_page(heap_t* heap, uint32_t page_type, uint32_t size_class) {
	page_orphan_lock_acquire();
	page_t* page = (page_t*)atomic_load_explicit(&global_page_orphan_empty[page_type], memory_order_relaxed);
	if (page)
		atomic_store_explicit(&global_page_orphan_empty[page_type], (uintptr_t)page->next, memory_order_relaxed);
	page_orphan_lock_release();
	if (!page)
		return 0;
#if RPMALLOC_HEAP_STATISTICS && ENABLE_DECOMMIT
	if (!page->is_decommitted)
		heap->stats.committed_size += page->commit_size;
#endif
	if (heap_make_free_page_available(heap, size_class, page) != 0)
		return 0;
	return page;
}

//! Hand the available pages of a released thread heap to the orphan lists. A page queued to the heap pending
//  list by a cross-thread free stays with the heap, which retires it once a thread adopts the heap
static void
heap_orphan_pages(heap_t* heap) {
	for (uint32_t iclass = 0; iclass < SIZE_CLASS_COUNT; ++iclass) {
		// The blocks in the heap list are counted as used in their page, give them back to the page first, they
		// would otherwise stay unused and keep the page from being emptied until a thread adopted the heap
		block_t* block = heap->local_free[iclass];
		heap->local_free[iclass] = 0;
		while (block) {
			block_t* next = block->next;
			page_t* block_page = block_get_page(block);
			block->next = block_page->local_free;
			block_page->local_free = block;
			--block_page->block_used;
			if (block_page->is_full)
				page_full_to_available(block_page);
			block = next;
		}
		page_t* page = heap->page_available[iclass];
		if (!page)
			continue;
		page_t* keep = 0;
		page_t* keep_tail = 0;
		page_t* orphan = 0;
		page_t* orphan_tail = 0;
#if ENABLE_STATISTICS
		uint32_t orphan_used = 0;
#endif
		while (page) {
			page_t* next = page->next;
			unsigned long long token = atomic_load_explicit(&page->thread_free, memory_order_relaxed);
			int claimed = 0;
			test_yield_point();
			while (!claimed && !(token & (THREAD_FREE_QUEUED | THREAD_FREE_FULL)))
				claimed = atomic_compare_exchange_weak_explicit(&page->thread_free, &token, token | THREAD_FREE_QUEUED,
				                                                memory_order_acquire, memory_order_relaxed);
			page->next = 0;
			if (claimed) {
				page_set_key(page, PAGE_KEY_GENERIC);
				page->heap = &global_heap_orphan;
#if RPMALLOC_HEAP_STATISTICS && ENABLE_DECOMMIT
				heap->stats.committed_size -= page->commit_size;
#endif
				_rpmalloc_stat_add_used(heap, iclass, -(int32_t)page->block_used);
#if ENABLE_STATISTICS
				orphan_used += page->block_used;
#endif
				if (orphan_tail)
					orphan_tail->next = page;
				else
					orphan = page;
				orphan_tail = page;
			} else {
				if (keep_tail) {
					keep_tail->next = page;
					page->prev = keep_tail;
				} else {
					keep = page;
				}
				keep_tail = page;
			}
			page = next;
		}
		if (keep)
			keep->prev = keep_tail;
		heap->page_available[iclass] = keep;
		if (orphan) {
			page_orphan_lock_acquire();
			_rpmalloc_stat_add_used(&global_heap_orphan, iclass, orphan_used);
			orphan_tail->next = (page_t*)atomic_load_explicit(&global_page_orphan[iclass], memory_order_relaxed);
			atomic_store_explicit(&global_page_orphan[iclass], (uintptr_t)orphan, memory_order_relaxed);
			page_orphan_lock_release();
		}
	}
}

//! Orphan a full page of the given released heap a thread frees blocks into, returns 1 if orphaned, zero if the page
//! stays with its heap, and -1 if the page is no longer full, orphaned by another thread or taken back by its heap.
//  The pages of a released heap that were full when it was released are in none of its lists, and the blocks freed
//  into them would wait in the heap level list until a thread adopted the heap. The freeing thread takes the heap
//  lock, which keeps the heap from being adopted, and hands the page with the blocks in its thread free list to the
//  orphan list of its size class. A thread holding the lock leaves the page to its heap as before
static int
page_orphan_full(page_t* page, heap_t* heap, block_t* head, block_t** tail_link, uint32_t count) {
	test_yield_point();
	uintptr_t unlocked = 0;
	if (atomic_load_explicit(&global_heap_lock, memory_order_relaxed) ||
	    !atomic_compare_exchange_strong_explicit(&global_heap_lock, &unlocked, get_thread_id(), memory_order_acquire,
	                                             memory_order_relaxed))
		return 0;
	int orphaned = 0;
	if (atomic_load_explicit(&heap->orphaned, memory_order_relaxed) && !heap->first_class) {
		unsigned long long token = atomic_load_explicit(&page->thread_free, memory_order_relaxed);
		uint64_t list = page_block_to_thread_free_list(page, page_block_offset(page, head), count) | THREAD_FREE_QUEUED;
		*tail_link = 0;
		while ((token & THREAD_FREE_FULL) && !(token & THREAD_FREE_QUEUED) && !orphaned)
			orphaned = atomic_compare_exchange_weak_explicit(&page->thread_free, &token, list, memory_order_acq_rel,
			                                                 memory_order_relaxed);
		// Another thread orphaned the page, or its heap took it back, since the full flag was read
		if (!(token & THREAD_FREE_FULL))
			orphaned = -1;
		if (orphaned > 0) {
			page->heap = &global_heap_orphan;
#if RPMALLOC_HEAP_STATISTICS && ENABLE_DECOMMIT
			heap->stats.committed_size -= page->commit_size;
#endif
			_rpmalloc_stat_add_used(heap, page->size_class, -(int32_t)page->block_used);
			page_orphan_lock_acquire();
			_rpmalloc_stat_add_used(&global_heap_orphan, page->size_class, page->block_used);
			page->next = (page_t*)atomic_load_explicit(&global_page_orphan[page->size_class], memory_order_relaxed);
			atomic_store_explicit(&global_page_orphan[page->size_class], (uintptr_t)page, memory_order_relaxed);
			page_orphan_lock_release();
		}
	}
	atomic_store_explicit(&global_heap_lock, 0, memory_order_release);
	return orphaned;
}

//! Take an orphan page of the size class as an available page of the heap, returns 0 if there is none
static NOINLINE page_t*
heap_take_orphan_page(heap_t* heap, uint32_t size_class) {
	page_orphan_lock_acquire();
	page_t* page = (page_t*)atomic_load_explicit(&global_page_orphan[size_class], memory_order_relaxed);
	if (page) {
		atomic_store_explicit(&global_page_orphan[size_class], (uintptr_t)page->next, memory_order_relaxed);
		_rpmalloc_stat_add_used(&global_heap_orphan, size_class, -(int32_t)page->block_used);
	}
	page_orphan_lock_release();
	if (!page)
		return 0;
	_rpmalloc_stat_add_used(heap, size_class, page->block_used);
	page->heap = heap;
	page->is_full = 0;
#if RPMALLOC_FIRST_CLASS_HEAPS
	page->heap_first_class = 0;
#endif
#if RPMALLOC_HEAP_STATISTICS && ENABLE_DECOMMIT
	heap->stats.committed_size += page->commit_size;
#endif
	page_t* head = heap->page_available[size_class];
	page->next = head;
	page->prev = head ? head->prev : page;
	if (head)
		head->prev = page;
	heap->page_available[size_class] = page;
	if (page->has_aligned_block == 0)
		page_set_key(page, page_key(heap));
	// The heap is written before the queued flag is cleared, with release ordering, so a thread whose free queues
	// the page again pushes it to the pending list of the new heap
	unsigned long long token = atomic_load_explicit(&page->thread_free, memory_order_relaxed);
	test_yield_point();
	while (!atomic_compare_exchange_weak_explicit(&page->thread_free, &token, token & ~THREAD_FREE_QUEUED,
	                                              memory_order_release, memory_order_relaxed))
		;
	return page;
}

//! Trims the memory a released heap holds for reuse. Until another thread adopts the heap nothing
//  reuses that memory or runs the decommit window, so a heap that many short lived threads pass along
//  would otherwise keep it all. The empty medium-large and large pages kept as the last available
//  page of their size class are retired, and free pages of those types are decommitted down to below
//  the overflow threshold. Small and medium-small pages are cheap to keep and costly to fault back in,
//  they are left to the decommit window, which keeps running across heap owners
static void
heap_release_free_pages(heap_t* heap) {
	for (uint32_t iclass = 0; iclass < SIZE_CLASS_COUNT; ++iclass) {
		page_t* page = heap->page_available[iclass];
		if (!page || page->next || page->block_used || (page->page_type < PAGE_MEDIUM_LARGE) ||
		    (atomic_load_explicit(&page->thread_free, memory_order_relaxed) & THREAD_FREE_QUEUED))
			continue;
		heap->page_available[iclass] = 0;
		page->is_free = 1;
		page->is_zero = 0;
		page->next = heap->page_free[page->page_type];
		heap->page_free[page->page_type] = page;
		if (!page->next)
			heap->page_free_tail[page->page_type] = page;
		++heap->page_free_commit_count[page->page_type];
	}
	for (uint32_t itype = PAGE_MEDIUM_LARGE; itype <= PAGE_LARGE; ++itype) {
		heap->page_free_overflow_ms[itype] = 0;
		if (heap->page_free_commit_count[itype] >= global_page_free_overflow[itype])
			heap_page_free_decommit(heap, itype, global_page_free_overflow[itype] - 1);
	}
}

//! Releases a heap, by its exiting thread or as a first class heap, for reuse by another thread.
//  With other heaps already queued the heap is likely to stay unused for a while and is trimmed
//  right away. With the queue empty a thread is likely to adopt it shortly, so the trim is left to
//  the adopter rather than delaying the release while threads wait for a heap
static void
heap_release_trim(heap_t* heap) {
#if HUGE_CACHE_SLOT_COUNT
	huge_cache_collect(monotonic_time_ms());
#endif
	if (atomic_load_explicit(&heap->page_pending, memory_order_relaxed))
		heap_collect_pending_pages(heap);
	heap_lock_acquire();
	int queued = (global_heap_queue != 0);
	heap_lock_release();
	if (queued)
		heap_release_free_pages(heap);
	else
		heap->release_trim = 1;
	heap_release(heap);
}

static inline int
heap_make_free_page_available(heap_t* heap, uint32_t size_class, page_t* page) {
	page->size_class = (uint16_t)size_class;
	page->block_size = global_size_class[size_class].block_size;
	page->block_count = global_size_class[size_class].block_count;
	page->block_used = 0;
	page->block_initialized = 0;
	page->local_free = 0;
	page->is_full = 0;
	page->is_free = 0;
	page->has_aligned_block = 0;
	page->local_reuse = 0;
	page->remote_fed = 0;
#if RPMALLOC_FIRST_CLASS_HEAPS
	page->heap_first_class = (heap->first_class != 0);
#endif
	page->heap = heap;
	page_set_key(page, page_key(heap));
	page_t* head = heap->page_available[size_class];
	page->next = head;
	page->prev = head ? head->prev : page;
	atomic_store_explicit(&page->thread_free, 0, memory_order_release);
	if (head)
		head->prev = page;
	heap->page_available[size_class] = page;
	if (page->is_decommitted != 0)
		return page_commit_memory_pages(page);
	return 0;
}

//! Find or allocate a span for the given page type with the given size class
static inline span_t*
heap_get_span(heap_t* heap, page_type_t page_type) {
	// Fast path, available span for given page type
	if (EXPECTED(heap->span_partial[page_type] != 0))
		return heap->span_partial[page_type];

	// Fallback path, map more memory
	size_t offset = 0;
	size_t mapped_size = 0;
	span_t* span = memory_interface_map(SPAN_SIZE, SPAN_SIZE, &offset, &mapped_size);
#if RPMALLOC_HEAP_STATISTICS
	heap->stats.mapped_size += mapped_size;
#endif
	if (EXPECTED(span != 0)) {
		rpmalloc_assert(!((uintptr_t)span & (SPAN_SIZE - 1)),
		                "memory_map returned a block not aligned to the span size");
		thp_advise(span, SPAN_SIZE, page_type);
		_rpmalloc_stat_inc_span_map(heap, page_type);
		uint32_t page_count = 0;
		uint32_t page_size = 0;
		if (page_type == PAGE_SMALL) {
			page_count = SPAN_SIZE / SMALL_PAGE_SIZE;
			page_size = SMALL_PAGE_SIZE;
		} else if (page_type == PAGE_MEDIUM_SMALL) {
			page_count = SPAN_SIZE / MEDIUM_SMALL_PAGE_SIZE;
			page_size = MEDIUM_SMALL_PAGE_SIZE;
		} else if (page_type == PAGE_MEDIUM_LARGE) {
			page_count = SPAN_SIZE / MEDIUM_LARGE_PAGE_SIZE;
			page_size = MEDIUM_LARGE_PAGE_SIZE;
		} else {
			page_count = SPAN_SIZE / LARGE_PAGE_SIZE;
			page_size = LARGE_PAGE_SIZE;
		}
		// Commit the span header, the slot table and the first page structures for reserve-only spans
		// (fully committed spans commit nothing here); record the commit model on the span only after its
		// header is committed
		int commit_on_demand = os_commit_on_demand();
#if ENABLE_DECOMMIT
		if (commit_on_demand) {
			if (global_memory_interface->memory_commit(span, SPAN_METADATA_COMMIT_INITIAL) != 0) {
				global_memory_interface->memory_unmap(span, offset, mapped_size);
				return 0;
			}
		}
#endif
		span->commit_on_demand = (uint32_t)commit_on_demand;
		uint32_t commit_shift = 0;
		while (((size_t)1 << commit_shift) < global_config.page_size)
			++commit_shift;
		span->commit_shift = commit_shift;
		span->metadata_commit = SPAN_METADATA_COMMIT_INITIAL;
#if RPMALLOC_HEAP_STATISTICS
#if ENABLE_DECOMMIT
		heap->stats.committed_size += SPAN_METADATA_COMMIT_INITIAL;
#else
		heap->stats.committed_size += mapped_size;
#endif
#endif
		span->heap = heap;
		span->page_type = page_type;
		span->page_count = page_count;
		span->page_size = page_size;
		span->offset = (uint32_t)offset;
		span->mapped_size = mapped_size;
		// The first pages hold the span metadata
		span->page_initialized = span_first_page(span);

		heap->span_partial[page_type] = span;
	}

	return span;
}

static page_t*
heap_get_page(heap_t* heap, uint32_t size_class);

static void
block_deallocate(block_t* block);

//! Check if other threads freed blocks into full pages of the heap, of any page type. The lists share the
//  cache line of the heap that other threads write
static inline int
heap_has_thread_free(heap_t* heap) {
	uintptr_t pending = atomic_load_explicit(&heap->thread_free[PAGE_SMALL], memory_order_relaxed) |
	                    atomic_load_explicit(&heap->thread_free[PAGE_MEDIUM_SMALL], memory_order_relaxed) |
	                    atomic_load_explicit(&heap->thread_free[PAGE_MEDIUM_LARGE], memory_order_relaxed) |
	                    atomic_load_explicit(&heap->thread_free[PAGE_LARGE], memory_order_relaxed);
	return (pending != 0);
}

//! Taking a new page from a span decommits free pages of other page types the heap cannot reuse for it
//  and ends the decommit windows of the other page types that have run their delay
static inline void
heap_page_commit_check(heap_t* heap, uint32_t page_type) {
	if (heap_page_commit_trim_pending(heap, page_type))
		heap_page_commit_trim(heap, page_type);
	if (heap_page_free_overflow_other_pending(heap, page_type))
		heap_page_free_overflow_other(heap, page_type);
}

//! Collect the blocks other threads freed into full pages of the heap, of every page type. The pages become
//  available again, or free once all their blocks are freed. Must not be called from the free path: the
//  local frees here can free pages and run the decommit of free pages
static NOINLINE void
heap_collect_thread_free(heap_t* heap) {
	for (uint32_t itype = 0; itype < 4; ++itype) {
		if (!atomic_load_explicit(&heap->thread_free[itype], memory_order_relaxed))
			continue;
		// Claim the whole list in one exchange, concurrent pushes either land before (and are claimed) or after
		block_t* block = (block_t*)atomic_exchange_explicit(&heap->thread_free[itype], 0, memory_order_acquire);
		// A thread free chain is pushed as a run of blocks of one page, each run is put in the page free list
		// at once. The blocks are block starts of pages of this heap, or of pages the heap gave to the orphan lists
		// after a thread freeing into them found them full, which are freed into their pages again
		page_t* block_page = block ? block_get_page(block) : 0;
		while (block) {
			block_t* last = block;
			block_t* next_block = block->next;
			page_t* next_page = 0;
			uint32_t count = 1;
			while (next_block) {
				next_page = block_get_page(next_block);
				if (next_page != block_page)
					break;
				last = next_block;
				next_block = next_block->next;
				++count;
			}
			if (EXPECTED(block_page->heap == heap))
				page_put_thread_free_run(block_page, block, last, count);
			else
				page_put_thread_free_chain(block_page, block, &last->next, count);
			block = next_block;
			block_page = next_page;
		}
	}
}

static page_t*
heap_get_page_generic(heap_t* heap, uint32_t size_class) {
	page_type_t page_type = get_page_type(size_class);
	if (UNEXPECTED(global_thread.chain_count != 0))
		thread_free_chain_flush(&global_thread);
#if HUGE_CACHE_SLOT_COUNT
	// The page refill path is taken regularly by any allocating thread, release cached huge mappings gone idle
	huge_cache_collect_idle();
#endif

	// Collect the blocks other threads freed into full pages, of all page types and not only the type needed
	// here, so a heap that stopped allocating a page type still reclaims and decommits those pages. Repeat
	// while frees keep arriving and no page of the size class is available, as a loop rather than a
	// recursion through heap_get_page
	while (UNEXPECTED(heap_has_thread_free(heap))) {
		heap_collect_thread_free(heap);
		page_t* page = heap->page_available[size_class];
		if (page)
			return page;
	}

	// Take a partly used orphan page before a free page or a new page. Its free blocks would otherwise stay unused,
	// while a free page left unused is decommitted after the decommit delay
	page_t* page = heap->page_free[page_type];
	if (UNEXPECTED(atomic_load_explicit(&global_page_orphan[size_class], memory_order_relaxed) != 0) && heap->id &&
	    !heap->first_class) {
		page_t* orphan = heap_take_orphan_page(heap, size_class);
		if (orphan)
			return orphan;
	}

	// Check if there is a free page
	if (EXPECTED(page != 0)) {
		heap->page_free[page_type] = page->next;
		if (!page->next)
			heap->page_free_tail[page_type] = 0;
		int recommit = (page->is_decommitted != 0);
		if (!recommit) {
			rpmalloc_assert(heap->page_free_commit_count[page_type] > 0, "Free committed page count out of sync");
			heap_page_free_commit_dec(heap, page_type);
		}
		if (heap_make_free_page_available(heap, size_class, page) != 0)
			return 0;
		if (recommit && heap_page_free_overflow_other_pending(heap, page_type))
			heap_page_free_overflow_other(heap, page_type);
		return page;
	}
	rpmalloc_assert(heap->page_free_commit_count[page_type] == 0, "Free committed page count out of sync");

	if (heap->id == 0) {
		// Thread has not yet initialized, or left its heap idle, assign heap and try again
		rpmalloc_initialize(0);
		return heap_get_page(get_thread_heap(), size_class);
	}
	++heap->activity;

	// A thread heap takes an empty orphan page of the page type before a new page from its spans
	if (UNEXPECTED(atomic_load_explicit(&global_page_orphan_empty[page_type], memory_order_relaxed) != 0) &&
	    !heap->first_class) {
		page = heap_take_orphan_empty_page(heap, page_type, size_class);
		if (page) {
			heap_page_commit_check(heap, page_type);
			return page;
		}
	}

	// Fallback path, find or allocate span for given size class
	// If thread was not initialized, the heap for the new span
	// will be different from the local heap variable in this scope
	// (which is the default heap) - so use span page heap instead
	span_t* span = heap_get_span(heap, page_type);
	if (EXPECTED(span != 0)) {
		page = span_allocate_page(span);
		if (!page || (heap_make_free_page_available(page->heap, size_class, page) != 0))
			return 0;
		heap_page_commit_check(page->heap, page_type);
	}

	return page;
}

//! Find or allocate a page for the given size class
static page_t*
heap_get_page(heap_t* heap, uint32_t size_class) {
	// Fast path, available page for given size class
	page_t* page = heap->page_available[size_class];
	if (EXPECTED(page != 0))
		return page;
	return heap_get_page_generic(heap, size_class);
}

//! Pop a block from the heap local free list
static inline RPMALLOC_ALLOCATOR void*
heap_pop_local_free(heap_t* heap, uint32_t size_class) {
	block_t** free_list = heap->local_free + size_class;
	block_t* block = *free_list;
	if (EXPECTED(block != 0))
		*free_list = block->next;
	return block;
}

//! Take the pages other threads queued from the pending page list. A page whose used blocks are all in its thread
//  free list is retired, or kept as the last available page of its size class with the blocks in its local free
//  list. The queued flag of the other pages is cleared, their blocks are adopted when the heap allocates from them
static NOINLINE void
heap_collect_pending_pages(heap_t* heap) {
	page_t* page = (page_t*)atomic_exchange_explicit(&heap->page_pending, 0, memory_order_acquire);
	while (page) {
		page_t* next = page->pending_next;
		unsigned long long token = atomic_load_explicit(&page->thread_free, memory_order_acquire);
		uint32_t count;
		int retire;
		do {
			test_yield_point();
			count = (uint32_t)((token >> 32ULL) & THREAD_FREE_COUNT_MASK);
			retire = !(token & THREAD_FREE_FULL) && (count == page->block_used);
		} while (!atomic_compare_exchange_weak_explicit(&page->thread_free, &token,
		                                                retire ? 0 : (token & ~THREAD_FREE_QUEUED),
		                                                memory_order_acq_rel, memory_order_acquire));
		if (retire) {
			if (count) {
				block_t* block;
				page_block_from_thread_free_list(page, token, &block);
				if ((heap->page_available[page->size_class] == page) && !page->next) {
					// Kept as the last available page, its free blocks must all be in the local free list
					if (page->local_free) {
						block_t* tail = block;
						while (tail->next)
							tail = tail->next;
						tail->next = page->local_free;
					}
					page->local_free = block;
				}
				page->remote_fed = 1;
				page->block_used = 0;
				_rpmalloc_stat_add_free(heap, page->size_class, count);
			}
			page_available_to_free(page);
		}
		page = next;
	}
}

//! Generic allocation path from heap pages, spans or new mapping
static NOINLINE RPMALLOC_ALLOCATOR void*
heap_allocate_block_small_to_large(heap_t* heap, uint32_t size_class, size_t zero_size) {
	page_t* page;
	uint32_t retain = PAGE_FULL_RETAIN;
	if (UNEXPECTED(atomic_load_explicit(&heap->page_pending, memory_order_relaxed) != 0))
		heap_collect_pending_pages(heap);
	while ((page = heap_get_page(heap, size_class)) != 0) {
		void* block = page_allocate_block(page, zero_size);
		if (EXPECTED(block != 0)) {
			// Count on the page owner, which may differ from heap when an uninitialized thread is routed
			// to its real heap, so the allocation and its later free are accounted on the same heap
			_rpmalloc_stat_inc_alloc(page->heap, size_class);
			return block;
		}
#if ENABLE_DECOMMIT
		// A page with blocks left to initialize only fails to allocate when committing their memory failed
		if (UNEXPECTED(page->block_initialized != page->block_count))
			return 0;
#endif
		// Running out of a page is a flush point of the thread free chain, bounding how long the blocks
		// an allocating thread freed into other heaps stay hidden from their owner, at about one check per
		// page of allocations
		if (UNEXPECTED(global_thread.chain_count != 0))
			thread_free_chain_flush(&global_thread);
		// The page has no free block. Move it to the back of the list a limited number of times so it
		// collects frees without leaving the list, then mark pages full
		heap = page->heap;
		if (retain && page->next && page->local_reuse && !page->remote_fed) {
			--retain;
			page_available_rotate(heap, page);
		} else {
			// Flag the page full in the thread free token, unless a cross-thread free just added a block
			// in which case the page stays available and is retried
			unsigned long long empty =
			    atomic_load_explicit(&page->thread_free, memory_order_relaxed) & THREAD_FREE_QUEUED;
			if (atomic_compare_exchange_strong_explicit(&page->thread_free, &empty, empty | THREAD_FREE_FULL,
			                                            memory_order_acq_rel, memory_order_relaxed))
				page_available_to_full(page);
		}
	}
	return 0;
}

//! Generic allocation path from heap pages, spans or new mapping
static NOINLINE RPMALLOC_ALLOCATOR void*
heap_allocate_block_huge(heap_t* heap, size_t size, size_t zero_size) {
	if (global_thread.chain_count != 0)
		thread_free_chain_flush(&global_thread);
	if (heap->id == 0) {
		// Thread has not yet initialized, assign heap and try again
		rpmalloc_initialize(0);
		heap = get_thread_heap();
	}
	size_t alloc_size = get_huge_aligned_size(size + HUGE_HEADER_SIZE);
	size_t commit_size = alloc_size;
	size_t clear_size = zero_size;
	span_t* span = 0;
	int from_cache = 0;
#if HUGE_CACHE_SLOT_COUNT
	// Reuse a recently freed huge mapping if one fits (the cached span keeps its own offset, mapped size and
	// commit model), committing or decommitting its tail to the committed size the cache gave it
	span = huge_cache_pop(alloc_size, &commit_size);
	if (span) {
		from_cache = 1;
#if OS_DECOMMIT_ZEROES
		// The pages of the mapping beyond its committed size were decommitted, or never committed, and read as
		// zero once committed, only the committed pages can hold old content
		size_t dirty_size = span_huge_committed(span) - HUGE_HEADER_SIZE;
		if ((global_memory_interface->memory_map == os_mmap) && (dirty_size < clear_size))
			clear_size = dirty_size;
#endif
#if ENABLE_DECOMMIT
		size_t committed = span_huge_committed(span);
		if (commit_size > committed) {
			rpmalloc_assert(commit_size == alloc_size, "Huge cache kept slack on a mapping committed up");
			void* tail = pointer_offset(span, committed);
			if (global_memory_interface->memory_commit(tail, commit_size - committed) != 0) {
				// Unmap the cached mapping and map a fresh one instead. A mapping committed up to the request is
				// accounted with the request size, as a fresh mapping is
				global_memory_interface->memory_unmap(span, span->offset, span->mapped_size);
				span = 0;
				from_cache = 0;
				clear_size = zero_size;
			}
		} else if (commit_size < committed) {
			void* tail = pointer_offset(span, commit_size);
			if (global_memory_interface->memory_decommit(tail, committed - commit_size) != 0) {
				huge_cache_live_add(committed - commit_size, 0);
				commit_size = committed;
			}
		}
#endif
	}
#endif
	if (!span) {
		size_t offset = 0;
		size_t mapped_size = 0;
		int commit_on_demand = os_commit_on_demand();
		size_t reserve_size = commit_on_demand ? os_huge_reserve_size(alloc_size) : alloc_size;
		span = memory_interface_map(reserve_size, SPAN_SIZE, &offset, &mapped_size);
#if ENABLE_DECOMMIT
		if (span && commit_on_demand && (global_memory_interface->memory_commit(span, alloc_size) != 0)) {
			global_memory_interface->memory_unmap(span, offset, mapped_size);
			span = 0;
		}
#endif
		if (!span) {
#if HUGE_CACHE_SLOT_COUNT
			huge_cache_live_sub(alloc_size, alloc_size);
#endif
			return 0;
		}
		rpmalloc_assert(!((uintptr_t)span & (SPAN_SIZE - 1)),
		                "memory_map returned a block not aligned to the span size");
		_rpmalloc_stat_inc_span_map(heap, PAGE_HUGE);
		thp_advise(span, reserve_size, PAGE_HUGE);
		// Huge blocks never use the per-page commit path, the flag records whether the mapping is reserve-only
		// so the cache can commit and decommit its tail
		span->commit_on_demand = (uint32_t)commit_on_demand;
		span->page_size = (uint32_t)global_config.page_size;
		span->offset = (uint32_t)offset;
		span->mapped_size = mapped_size;
	}
	span->heap = heap;
	span->page_type = PAGE_HUGE;
	span->page_count = (uint32_t)(commit_size / span->page_size);
	span->page_request = (uint32_t)(alloc_size / span->page_size);
	// The huge block page always takes the generic free path, which releases the span
	page_t* page = span_get_huge_page(span);
	memset(page, 0, sizeof(page_t));
	page->heap = heap;
	page->is_full = 1;
	page->page_type = PAGE_HUGE;
	page->block_start = pointer_offset(span, HUGE_HEADER_SIZE);
	span_set_huge_slots(span);
#if RPMALLOC_HEAP_STATISTICS
	heap->stats.mapped_size += span->mapped_size;
#if ENABLE_DECOMMIT
	heap->stats.committed_size += commit_size;
#else
	heap->stats.committed_size += span->mapped_size;
#endif
	heap->stats.allocated_size += alloc_size;
#endif
#if ENABLE_STATISTICS
	statistics_add_huge_alloc(commit_size);
#endif
	// Keep track of span if first class heap
	if (!heap->owner_thread)
		span_huge_track(span);
	void* ptr = pointer_offset(span, HUGE_HEADER_SIZE);
	// A fresh mapping from the default OS interface is already zero filled (anonymous mmap and
	// VirtualAlloc both guarantee it), clearing it again would fault in and commit every page.
	// Reused cached mappings and custom memory interfaces give no such guarantee, but the pages a
	// reused mapping commits beyond its previously committed pages are zero where decommit zeroes.
	if (clear_size && (from_cache || (global_memory_interface->memory_map != os_mmap)))
		memset(ptr, 0, clear_size);
	return ptr;
}

static RPMALLOC_ALLOCATOR NOINLINE void*
heap_allocate_block_generic(heap_t* heap, size_t size, size_t zero_size) {
	uint32_t size_class = get_size_class(size);
	if (EXPECTED(size_class < SIZE_CLASS_COUNT)) {
#if RPMALLOC_HEAP_STATISTICS
		heap->stats.allocated_size += global_size_class[size_class].block_size;
#endif

		block_t* block = heap_pop_local_free(heap, size_class);
		if (EXPECTED(block != 0)) {
			// Fast track with small block available in heap level local free list; a hit means this
			// heap owns the block
			_rpmalloc_stat_inc_alloc(heap, size_class);
			if (zero_size)
				memset(block, 0, zero_size);
			return block;
		}

		return heap_allocate_block_small_to_large(heap, size_class, zero_size);
	}

	return heap_allocate_block_huge(heap, size, zero_size);
}

//! Find or allocate a block of the given size. The first zero_size bytes of the block are zeroed, the
//  requested size for calloc and zalloc rather than the whole size class block: the bytes past the
//  request are not part of the allocation
static inline RPMALLOC_ALLOCATOR void*
heap_allocate_block(heap_t* heap, size_t size, size_t zero_size) {
	if (size <= (SMALL_GRANULARITY * 64)) {
		uint32_t size_class = get_size_class_tiny(size);
		block_t* block = heap_pop_local_free(heap, size_class);
		if (EXPECTED(block != 0)) {
			// Fast track with small block available in heap level local free list
			if (zero_size)
				memset(block, 0, zero_size);
#if RPMALLOC_HEAP_STATISTICS
			heap->stats.allocated_size += global_size_class[size_class].block_size;
#endif
			_rpmalloc_stat_inc_alloc(heap, size_class);
			return block;
		}
#if RPMALLOC_HEAP_STATISTICS
		heap->stats.allocated_size += global_size_class[size_class].block_size;
#endif
		// The size class is already known - refill directly, skipping the
		// generic path size class recomputation and dead local free list pop
		// (small_to_large counts the allocation on the owning heap)
		return heap_allocate_block_small_to_large(heap, size_class, zero_size);
	}
	return heap_allocate_block_generic(heap, size, zero_size);
}

static RPMALLOC_ALLOCATOR void*
heap_allocate_block_aligned(heap_t* heap, size_t alignment, size_t size, unsigned int zero) {
	if (alignment <= SMALL_GRANULARITY)
		return heap_allocate_block(heap, size, zero ? size : 0);

#if ENABLE_VALIDATE_ARGS
	if ((size + alignment) < size) {
		errno = EINVAL;
		return 0;
	}
	if (alignment & (alignment - 1)) {
		errno = EINVAL;
		return 0;
	}
#endif
	if (alignment >= RPMALLOC_MAX_ALIGNMENT) {
		errno = EINVAL;
		return 0;
	}

	size_t align_mask = alignment - 1;
	// Blocks start at the page start, which is aligned to the page size. Rounding the size up to a multiple of
	// the alignment (and at least the alignment) selects a size class whose block size is a multiple of the
	// alignment: up to 1KiB every multiple of the granularity is a class, above that the classes of each power
	// of two range are multiples of a quarter of its lower bound, and the multiple of the alignment is in the
	// range or at its lower bound. The page size of the class is at least the block size, so every block of
	// that class is naturally aligned, there is no need to over-allocate and realign, and the page keeps the
	// fast free path. A huge block starts at the huge block header size offset from the span start
	size_t aligned_size = (size + align_mask) & ~align_mask;
	if (aligned_size < alignment)
		aligned_size = alignment;
	if ((aligned_size >= size) && ((aligned_size <= LARGE_BLOCK_SIZE_LIMIT) || (alignment <= HUGE_HEADER_SIZE))) {
		block_t* block = heap_allocate_block(heap, aligned_size, zero ? size : 0);
		rpmalloc_assert(!((uintptr_t)block & align_mask), "Natural block alignment internal failure");
		return block;
	}
	// The block is realigned by up to the alignment after allocation, zeroing the whole request from the
	// block start covers the realigned range and keeps fresh pages and mappings untouched
	block_t* block = heap_allocate_block(heap, size + alignment, zero ? (size + alignment) : 0);
	if ((uintptr_t)block & align_mask) {
		block = (void*)(((uintptr_t)block & ~(uintptr_t)align_mask) + alignment);
		// Mark as having aligned blocks, a huge block always takes the generic free path
		page_t* page = block_get_page(block);
		if (page->page_type != PAGE_HUGE) {
			page->has_aligned_block = 1;
			page_set_key(page, PAGE_KEY_GENERIC);
		}
	}
	return block;
}

//! Grow a huge block in place when its mapping has the address range for it, as a reused cached mapping
//  can. The committed slack is used first, then the pages up to the new size are committed. Returns the
//  block, or 0 if the mapping is too small.
static void*
span_huge_grow(span_t* span, size_t size) {
	size_t alloc_size = get_huge_aligned_size(size + HUGE_HEADER_SIZE);
	size_t old_request = (size_t)span->page_request * span->page_size;
	size_t old_committed = span_huge_committed(span);
	if ((alloc_size <= old_request) || (alloc_size > ((size_t)span->mapped_size - span->offset)) ||
	    (span->page_size != global_config.page_size))
		return 0;
	size_t committed = old_committed;
	if (alloc_size > old_committed) {
#if ENABLE_DECOMMIT
		void* tail = pointer_offset(span, old_committed);
		if (span->commit_on_demand && (global_memory_interface->memory_commit(tail, alloc_size - old_committed) != 0))
			return 0;
#endif
		committed = alloc_size;
		span->page_count = (uint32_t)(alloc_size / span->page_size);
	}
	span->page_request = (uint32_t)(alloc_size / span->page_size);
#if HUGE_CACHE_SLOT_COUNT
	huge_cache_live_add(committed - old_committed, alloc_size - old_request);
#else
	(void)sizeof(committed);
	(void)sizeof(old_request);
#endif
#if ENABLE_STATISTICS
	statistics_add_huge_alloc(committed - old_committed);
#endif
#if RPMALLOC_HEAP_STATISTICS
#if ENABLE_DECOMMIT
	span->heap->stats.committed_size += committed - old_committed;
#endif
	span->heap->stats.allocated_size += alloc_size - old_request;
#endif
	return pointer_offset(span, HUGE_HEADER_SIZE);
}

//! Shrink a huge block in place to a smaller huge size. The committed pages beyond twice the new size are
//  decommitted, keeping the same slack as a reused cached mapping. Returns the block, or 0 if the mapping is from
//  before a reinitialization with another page size.
static void*
span_huge_shrink(span_t* span, size_t size) {
	size_t alloc_size = get_huge_aligned_size(size + HUGE_HEADER_SIZE);
	size_t old_request = (size_t)span->page_request * span->page_size;
	size_t old_committed = span_huge_committed(span);
	if ((alloc_size >= old_request) || (span->page_size != global_config.page_size))
		return 0;
	size_t committed = old_committed;
#if ENABLE_DECOMMIT
	size_t slack_limit = alloc_size * 2;
	if (span->commit_on_demand && (old_committed > slack_limit)) {
		void* tail = pointer_offset(span, slack_limit);
		if (global_memory_interface->memory_decommit(tail, old_committed - slack_limit) == 0) {
			committed = slack_limit;
			span->page_count = (uint32_t)(slack_limit / span->page_size);
		}
	}
#endif
	span->page_request = (uint32_t)(alloc_size / span->page_size);
#if HUGE_CACHE_SLOT_COUNT
	huge_cache_live_sub(old_committed - committed, old_request - alloc_size);
#else
	(void)sizeof(committed);
	(void)sizeof(old_request);
#endif
#if ENABLE_STATISTICS
	statistics_sub_saturating(&global_statistics.huge_alloc, old_committed - committed);
#endif
#if RPMALLOC_HEAP_STATISTICS
#if ENABLE_DECOMMIT
	span->heap->stats.committed_size -= old_committed - committed;
#endif
	span->heap->stats.allocated_size -= old_request - alloc_size;
#endif
	return pointer_offset(span, HUGE_HEADER_SIZE);
}

#if defined(__linux__) && defined(MREMAP_MAYMOVE) && defined(MREMAP_FIXED)
#define ENABLE_HUGE_REMAP 1
#else
#define ENABLE_HUGE_REMAP 0
#endif

#if ENABLE_HUGE_REMAP
//! Grow a huge block by moving its mapping into a new, larger span aligned reservation with mremap.
//  The kernel moves the page table entries instead of copying the content, so growing a large buffer
//  costs no memory copy and no page faults on the moved range. Returns the new block, or 0 if the
//  block cannot be remapped (the caller then falls back to allocate, copy and free).
static void*
span_huge_remap(span_t* span, size_t size) {
	// Only plain mappings from the default OS interface can be moved: huge page (hugetlb) mappings have
	// stricter remap rules, and spans tracked in a first class heap span list cannot change address
	if ((global_memory_interface->memory_map != os_mmap) || os_huge_pages || span->offset ||
	    span_get_huge_page(span)->is_tracked)
		return 0;
	size_t alloc_size = get_huge_aligned_size(size + HUGE_HEADER_SIZE);
	size_t old_mapped_size = span->mapped_size;
	if (alloc_size <= old_mapped_size)
		return 0;
	size_t offset = 0;
	size_t mapped_size = 0;
	size_t reserve_size = os_commit_on_demand() ? os_huge_reserve_size(alloc_size) : alloc_size;
	void* target = os_mmap(reserve_size, SPAN_SIZE, &offset, &mapped_size);
	if (!target)
		return 0;
	void* moved = mremap(span, old_mapped_size, mapped_size, MREMAP_MAYMOVE | MREMAP_FIXED, target);
	if (moved != target) {
		os_munmap(target, offset, mapped_size);
		return 0;
	}
	span = target;
	// The slot table entries and the page hold addresses in the old mapping
	span_get_huge_page(span)->block_start = pointer_offset(span, HUGE_HEADER_SIZE);
	span_set_huge_slots(span);
#if ENABLE_DECOMMIT
	if (os_commit_on_demand())
		global_memory_interface->memory_commit(span, alloc_size);
#endif
	size_t old_committed = span_huge_committed(span);
	size_t old_request = (size_t)span->page_request * span->page_size;
#if ENABLE_STATISTICS
	// The old mapping is gone without passing through os_munmap, account for it the same way
	statistics_sub_saturating(&global_statistics.page_mapped, old_mapped_size / global_config.page_size);
	statistics_sub_saturating(&global_statistics.page_active, old_mapped_size / global_config.page_size);
	statistics_sub_saturating(&global_statistics.huge_alloc, old_committed);
#endif
	span->page_count = (uint32_t)(alloc_size / span->page_size);
	span->page_request = span->page_count;
	span->mapped_size = mapped_size;
	size_t new_committed = span_huge_committed(span);
#if HUGE_CACHE_SLOT_COUNT
	huge_cache_live_add(new_committed - old_committed, new_committed - old_request);
#else
	(void)sizeof(old_request);
#endif
#if ENABLE_STATISTICS
	statistics_add_huge_alloc(new_committed);
#endif
#if RPMALLOC_HEAP_STATISTICS
	span->heap->stats.mapped_size += mapped_size - old_mapped_size;
	span->heap->stats.committed_size += new_committed - old_committed;
	span->heap->stats.allocated_size += new_committed - old_request;
#else
	(void)sizeof(old_committed);
	(void)sizeof(new_committed);
#endif
	return pointer_offset(span, HUGE_HEADER_SIZE);
}
#endif

static NOINLINE void*
heap_reallocate_block(heap_t* heap, void* block, size_t size, size_t old_size, unsigned int flags) {
	if (block) {
		page_t* page = block_get_page(block);
		if (EXPECTED(page->page_type <= PAGE_LARGE)) {
			// Normal sized block
			// A pointer from the allocator is at its block start unless the page holds aligned blocks,
			// so the division locating the block start is only needed for those pages
			void* block_origin = block;
			if (UNEXPECTED(page->has_aligned_block))
				block_origin = page_block_realign(page, block);
			if (!old_size)
				old_size = (size_t)((ptrdiff_t)page->block_size - pointer_diff(block, block_origin));
			if ((size_t)page->block_size >= size) {
				// Still fits in block, never mind trying to save memory, but preserve data if alignment changed
				if ((block != block_origin) && !(flags & RPMALLOC_NO_PRESERVE))
					memmove(block_origin, block, old_size);
				return block_origin;
			}
		} else {
			// Huge block
			span_t* span = block_get_span(block);
			void* block_start = pointer_offset(span, HUGE_HEADER_SIZE);
			if (!old_size)
				old_size = ((size_t)span->page_size * (size_t)span->page_request) - (size_t)pointer_diff(block, span);
			if ((size < old_size) && (size > LARGE_BLOCK_SIZE_LIMIT)) {
				if (size >= (old_size / 2)) {
					// Still fits in block, still huge and saves less than half the memory,
					// never mind trying to save memory, but preserve data if alignment changed
					if ((block_start != block) && !(flags & RPMALLOC_NO_PRESERVE))
						memmove(block_start, block, old_size);
					return block_start;
				}
				// Saves at least half the memory and stays huge, shrink in place instead of moving the block,
				// unless it is an aligned block within its mapping
				if (block == block_start) {
					void* shrunk_block = span_huge_shrink(span, size);
					if (shrunk_block)
						return shrunk_block;
				}
			}
		}
	} else {
		old_size = 0;
	}

	if (!!(flags & RPMALLOC_GROW_OR_FAIL))
		return 0;

	// Size is greater than block size or saves enough memory to resize, need to allocate a new block
	// and deallocate the old. Avoid hysteresis by overallocating if increase is small (below 37%)
	size_t lower_bound = old_size + (old_size >> 2) + (old_size >> 3);
	size_t new_size = (size > lower_bound) ? size : ((size > old_size) ? lower_bound : size);
	if (block && (new_size > old_size) && (new_size > LARGE_BLOCK_SIZE_LIMIT)) {
		span_t* span = block_get_span(block);
		if ((span->page_type == PAGE_HUGE) && (block == pointer_offset(span, HUGE_HEADER_SIZE))) {
			void* grown_block = span_huge_grow(span, new_size);
			if (!grown_block && (size < new_size))
				grown_block = span_huge_grow(span, size);
#if ENABLE_HUGE_REMAP
			if (!grown_block)
				grown_block = span_huge_remap(span, new_size);
#endif
			if (grown_block)
				return grown_block;
		}
	}
	void* old_block = block;
	block = heap_allocate_block(heap, new_size, 0);
	if (block && old_block) {
		if (!(flags & RPMALLOC_NO_PRESERVE))
			memcpy(block, old_block, old_size < new_size ? old_size : new_size);
		block_deallocate(old_block);
	}

	return block;
}

static void*
heap_reallocate_block_aligned(heap_t* heap, void* block, size_t alignment, size_t size, size_t old_size,
                              unsigned int flags) {
	if (alignment <= SMALL_GRANULARITY)
		return heap_reallocate_block(heap, block, size, old_size, flags);

	int no_alloc = !!(flags & RPMALLOC_GROW_OR_FAIL);
	size_t usable_size = (block ? block_usable_size(block) : 0);
	if (block && (usable_size >= size) && !((uintptr_t)block & (alignment - 1))) {
		if (no_alloc || (size >= (usable_size / 2)))
			return block;
	}
	// Aligned alloc marks span as having aligned blocks
	void* old_block = block;
	block = (!no_alloc ? heap_allocate_block_aligned(heap, alignment, size, 0) : 0);
	if (EXPECTED(block != 0)) {
		if (!(flags & RPMALLOC_NO_PRESERVE) && old_block) {
			if (!old_size)
				old_size = usable_size;
			memcpy(block, old_block, old_size < size ? old_size : size);
		}
		if (EXPECTED(old_block != 0))
			block_deallocate(old_block);
	}
	return block;
}

static void
heap_free_all(heap_t* heap) {
	for (int itype = 0; itype < 4; ++itype) {
		span_t* span = heap->span_partial[itype];
		while (span) {
			span_t* span_next = span->next;
			global_memory_interface->memory_unmap(span, span->offset, span->mapped_size);
			span = span_next;
		}
		heap->span_partial[itype] = 0;
		heap->page_free[itype] = 0;
		heap->page_free_tail[itype] = 0;
		heap->page_free_commit_count[itype] = 0;
		heap->page_free_overflow_ms[itype] = 0;
		atomic_store_explicit(&heap->thread_free[itype], 0, memory_order_release);
	}
	atomic_store_explicit(&heap->page_pending, 0, memory_order_release);
	heap->page_commit_credit = 0;
	for (int itype = 0; itype < 5; ++itype) {
		span_t* span = heap->span_used[itype];
		while (span) {
			span_t* span_next = span->next;
			if (itype == PAGE_HUGE) {
#if ENABLE_STATISTICS
				statistics_sub_saturating(&global_statistics.huge_alloc, span_huge_committed(span));
#endif
#if HUGE_CACHE_SLOT_COUNT
				huge_cache_live_sub(span_huge_committed(span), (size_t)span->page_request * span->page_size);
#endif
			}
			global_memory_interface->memory_unmap(span, span->offset, span->mapped_size);
			span = span_next;
		}
		heap->span_used[itype] = 0;
	}
	memset(heap->local_free, 0, sizeof(heap->local_free));
	memset(heap->page_available, 0, sizeof(heap->page_available));

#if ENABLE_STATISTICS
	// Every block in this heap has been released, so reverse the per size class allocation counters
	for (uint32_t iclass = 0; iclass < SIZE_CLASS_COUNT; ++iclass) {
		heap->size_use[iclass].free_total += heap->size_use[iclass].alloc_current;
		heap->size_use[iclass].alloc_current = 0;
	}
#endif
}

////////////
///
/// Extern interface
///
//////

int
rpmalloc_is_thread_initialized(void) {
	// A thread with its heap idle holds the heap apart from the thread local pointer
	return ((get_thread_heap() != global_heap_default) || global_thread.idle_heap) ? 1 : 0;
}

extern inline RPMALLOC_ALLOCATOR ENTRY_ALIGNED void*
rpmalloc(size_t size) {
#if ENABLE_VALIDATE_ARGS
	if (size >= MAX_ALLOC_SIZE) {
		errno = EINVAL;
		return 0;
	}
#endif
	heap_t* heap = get_thread_heap();
	return heap_allocate_block(heap, size, 0);
}

extern inline RPMALLOC_ALLOCATOR ENTRY_ALIGNED void*
rpzalloc(size_t size) {
#if ENABLE_VALIDATE_ARGS
	if (size >= MAX_ALLOC_SIZE) {
		errno = EINVAL;
		return 0;
	}
#endif
	heap_t* heap = get_thread_heap();
	return heap_allocate_block(heap, size, size);
}

extern inline ENTRY_ALIGNED void
rpfree(void* ptr) {
	if (UNEXPECTED(ptr == 0))
		return;
	block_deallocate(ptr);
}

extern inline RPMALLOC_ALLOCATOR ENTRY_ALIGNED void*
rpcalloc(size_t num, size_t size) {
	size_t total;
	if (size_mul_overflow(num, size, &total)) {
		errno = ENOMEM;
		return 0;
	}
#if ENABLE_VALIDATE_ARGS
	if (total >= MAX_ALLOC_SIZE) {
		errno = EINVAL;
		return 0;
	}
#endif
	heap_t* heap = get_thread_heap();
	return heap_allocate_block(heap, total, total);
}

extern inline RPMALLOC_ALLOCATOR ENTRY_ALIGNED void*
rprealloc(void* ptr, size_t size) {
#if ENABLE_VALIDATE_ARGS
	if (size >= MAX_ALLOC_SIZE) {
		errno = EINVAL;
		return ptr;
	}
#endif
	if (EXPECTED(ptr != 0)) {
		// Fast path, a block from a page without aligned blocks that still fits the size is kept as is
		page_t* page = block_get_page(ptr);
		if (EXPECTED((page->page_type <= PAGE_LARGE) && !page->has_aligned_block && ((size_t)page->block_size >= size)))
			return ptr;
	}
	heap_t* heap = get_thread_heap();
	return heap_reallocate_block(heap, ptr, size, 0, 0);
}

extern RPMALLOC_ALLOCATOR void*
rpaligned_realloc(void* ptr, size_t alignment, size_t size, size_t oldsize, unsigned int flags) {
#if ENABLE_VALIDATE_ARGS
	if ((size + alignment < size) || (alignment > SMALL_PAGE_SIZE)) {
		errno = EINVAL;
		return 0;
	}
#endif
	heap_t* heap = get_thread_heap();
	return heap_reallocate_block_aligned(heap, ptr, alignment, size, oldsize, flags);
}

extern RPMALLOC_ALLOCATOR void*
rpaligned_alloc(size_t alignment, size_t size) {
	heap_t* heap = get_thread_heap();
	return heap_allocate_block_aligned(heap, alignment, size, 0);
}

extern RPMALLOC_ALLOCATOR void*
rpaligned_zalloc(size_t alignment, size_t size) {
	heap_t* heap = get_thread_heap();
	return heap_allocate_block_aligned(heap, alignment, size, 1);
}

extern inline RPMALLOC_ALLOCATOR void*
rpaligned_calloc(size_t alignment, size_t num, size_t size) {
	size_t total;
	if (size_mul_overflow(num, size, &total)) {
		errno = ENOMEM;
		return 0;
	}
#if ENABLE_VALIDATE_ARGS
	if (total >= MAX_ALLOC_SIZE) {
		errno = EINVAL;
		return 0;
	}
#endif
	heap_t* heap = get_thread_heap();
	return heap_allocate_block_aligned(heap, alignment, total, 1);
}

extern inline RPMALLOC_ALLOCATOR void*
rpmemalign(size_t alignment, size_t size) {
	heap_t* heap = get_thread_heap();
	return heap_allocate_block_aligned(heap, alignment, size, 0);
}

extern inline int
rpposix_memalign(void** memptr, size_t alignment, size_t size) {
	heap_t* heap = get_thread_heap();
	if (memptr)
		*memptr = heap_allocate_block_aligned(heap, alignment, size, 0);
	else
		return EINVAL;
	return *memptr ? 0 : ENOMEM;
}

extern inline ENTRY_ALIGNED size_t
rpmalloc_usable_size(void* ptr) {
	return (ptr ? block_usable_size(ptr) : 0);
}

extern void
rpmalloc_linker_reference(void) {
}

////////////
///
/// Initialization and finalization
///
//////

#define HEAP_STATE_ACTIVE 0
#define HEAP_STATE_IDLE 1
#define HEAP_STATE_REAPING 2

//! Take an idle heap back from the idle state, waiting for a reap holding it to finish
static NOINLINE void
heap_idle_acquire(heap_t* heap) {
	unsigned int state = HEAP_STATE_IDLE;
	uint32_t spin = 0;
	while (!atomic_compare_exchange_weak_explicit(&heap->idle_state, &state, HEAP_STATE_ACTIVE, memory_order_acquire,
	                                              memory_order_relaxed)) {
		rpmalloc_assert(state != HEAP_STATE_ACTIVE, "Idle heap taken back twice");
		if (state == HEAP_STATE_ACTIVE)
			return;
		if (state == HEAP_STATE_REAPING)
			wait_spin(&spin);
		state = HEAP_STATE_IDLE;
	}
}

//! Make the heap the calling thread left idle its thread heap again
static NOINLINE void
heap_idle_attach(heap_t* heap) {
	heap_idle_acquire(heap);
	global_thread.idle_heap = 0;
	// Only the thread local pointer was cleared, the platform key and owner thread still name the heap
	global_thread.heap = heap;
}

static void
rpmalloc_thread_destructor(void* value) {
	// If this is called on main thread assume it means rpmalloc_finalize
	// has not been called and shutdown is forced (through _exit) or unclean
	if (get_thread_id() == global_main_thread_id)
		return;
#if defined(__APPLE__)
	// dyld frees thread local storage in a key destructor that usually runs before this one, and touching it
	// allocates it again holding the fallback heap. Release the heap from the value instead, which is the heap of
	// the exiting thread as key destructors only run on the thread they belong to. Destructors run in key order,
	// and this key runs first if it reused an index below the key dyld created for the image. Thread local storage
	// then still holds the heap and is reset as in rpmalloc_thread_finalize, or destructors running later on this
	// thread would allocate from the released heap while another thread adopts it
	if (value && (value != global_heap_default)) {
		if (atomic_load_explicit(&((heap_t*)value)->idle_state, memory_order_relaxed) != HEAP_STATE_ACTIVE) {
			heap_idle_acquire(value);
			if (global_thread.idle_heap == value)
				global_thread.idle_heap = 0;
		}
		if (get_thread_heap() == value)
			set_thread_heap(global_heap_default);
		heap_release_trim(value);
	}
#else
	if (value)
		rpmalloc_thread_finalize();
#endif
}

#if PLATFORM_WINDOWS
//! FlsFree calls the destructor for every thread on the calling thread, which must do nothing once detached
static void NTAPI
rpmalloc_fls_destructor(void* value) {
	if (!global_module_detached)
		rpmalloc_thread_destructor(value);
}
#endif

#if PLATFORM_WINDOWS && !ENABLE_DYNAMIC_LINK && (defined(_MSC_VER) || defined(__MINGW32__))
//! Free the thread destructor slot when a DLL is unloaded, without finalizing as the DLL can still allocate
static void NTAPI
rpmalloc_tls_detach(PVOID module, DWORD reason, PVOID reserved) {
	(void)sizeof(reserved);
	if (reason != DLL_PROCESS_DETACH)
		return;
	global_module_detached = 1;
	// An executable only detaches at process exit and stays mapped
	const IMAGE_DOS_HEADER* dos_header = (const IMAGE_DOS_HEADER*)module;
	const IMAGE_NT_HEADERS* nt_headers = (const IMAGE_NT_HEADERS*)pointer_offset(module, dos_header->e_lfanew);
	if ((nt_headers->FileHeader.Characteristics & IMAGE_FILE_DLL) && (fls_key != FLS_OUT_OF_INDEXES)) {
		FlsFree(fls_key);
		fls_key = FLS_OUT_OF_INDEXES;
	}
}

// TLS callbacks are called in section name order, .CRT$XLY runs after the C runtime callbacks
#if defined(_MSC_VER)
#if defined(_M_IX86)
#pragma comment(linker, "/INCLUDE:__tls_used")
#pragma comment(linker, "/INCLUDE:_rpmalloc_tls_detach_callback")
#else
#pragma comment(linker, "/INCLUDE:_tls_used")
#pragma comment(linker, "/INCLUDE:rpmalloc_tls_detach_callback")
#endif
#pragma const_seg(".CRT$XLY")
extern const PIMAGE_TLS_CALLBACK rpmalloc_tls_detach_callback;
const PIMAGE_TLS_CALLBACK rpmalloc_tls_detach_callback = rpmalloc_tls_detach;
#pragma const_seg()
#else
extern const IMAGE_TLS_DIRECTORY _tls_used;
__attribute__((used)) static const void* const rpmalloc_tls_used_reference = &_tls_used;
extern PIMAGE_TLS_CALLBACK rpmalloc_tls_detach_callback;
__attribute__((used, section(".CRT$XLY"))) PIMAGE_TLS_CALLBACK rpmalloc_tls_detach_callback = rpmalloc_tls_detach;
#endif
#endif

#if PLATFORM_POSIX && (defined(__GNUC__) || defined(__clang__))
//! Delete the thread destructor key when the module is unloaded, without finalizing as the module can still allocate
__attribute__((destructor)) static void
rpmalloc_module_detach(void) {
	global_module_detached = 1;
	if (pthread_key_created) {
		pthread_key_delete(pthread_key);
		pthread_key_created = 0;
	}
}
#endif

#if defined(__linux__) || defined(__ANDROID__)
//! Read a small (pseudo) file such as /proc/meminfo into a caller provided stack buffer
//! using raw syscalls. This deliberately avoids stdio (fopen/fgets), which allocates an
//! internal buffer through malloc - during rpmalloc_initialize that allocation would
//! re-enter the allocator (when the malloc override is enabled) before the page size is
//! established. Returns the number of bytes read (buffer is always null terminated), or 0.
static size_t
os_read_system_file(const char* path, char* buffer, size_t capacity) {
	if (capacity < 2)
		return 0;
	int fd = open(path, O_RDONLY | O_CLOEXEC);
	if (fd < 0)
		return 0;
	size_t total = 0;
	while ((total + 1) < capacity) {
		ssize_t got = read(fd, buffer + total, (capacity - 1) - total);
		if (got <= 0)
			break;
		total += (size_t)got;
	}
	close(fd);
	buffer[total] = 0;
	return total;
}

//! Parse the decimal number at the start of a string, after any blanks
static size_t
os_parse_decimal(const char* text) {
	while ((*text == ' ') || (*text == '\t'))
		++text;
	size_t value = 0;
	while ((*text >= '0') && (*text <= '9'))
		value = (value * 10) + (size_t)(*text++ - '0');
	return value;
}
#endif

extern int
rpmalloc_initialize_config(rpmalloc_interface_t* memory_interface, rpmalloc_config_t* config) {
	if (atomic_load_explicit(&global_rpmalloc_init_state, memory_order_acquire) == RPMALLOC_INIT_DONE) {
		rpmalloc_thread_initialize();
		if (config)
			*config = global_config;
		return 0;
	}

	if (config)
		global_config = *config;

	int result = rpmalloc_initialize(memory_interface);

	if (config)
		*config = global_config;

	return result;
}

extern int
rpmalloc_initialize(rpmalloc_interface_t* memory_interface) {
	for (;;) {
		int state = atomic_load_explicit(&global_rpmalloc_init_state, memory_order_acquire);
		if (state == RPMALLOC_INIT_DONE) {
			rpmalloc_thread_initialize();
			return 0;
		}
		if (state == RPMALLOC_INIT_RUNNING) {
			while (atomic_load_explicit(&global_rpmalloc_init_state, memory_order_acquire) ==
			       RPMALLOC_INIT_RUNNING)
				thread_yield();
			continue;
		}
		int expected = RPMALLOC_INIT_UNINIT;
		if (atomic_compare_exchange_strong_explicit(&global_rpmalloc_init_state, &expected,
		                                            RPMALLOC_INIT_RUNNING, memory_order_acq_rel,
		                                            memory_order_acquire))
			break;
	}

	// Remember whether the caller explicitly requested huge pages, the detection below
	// overwrites global_config.enable_huge_pages with what is actually available.
	const int huge_pages_requested = global_config.enable_huge_pages;

	global_memory_interface = memory_interface ? memory_interface : &global_memory_interface_default;
	if (!global_memory_interface->memory_map || !global_memory_interface->memory_unmap) {
		global_memory_interface->memory_map = os_mmap;
		global_memory_interface->memory_commit = os_mcommit;
		global_memory_interface->memory_decommit = os_mdecommit;
		global_memory_interface->memory_unmap = os_munmap;
	}

#if ENABLE_OVERRIDE
	// With the standard library override, rpmalloc backs the C runtime's own allocations (for
	// example per-thread TLS allocated by the loader). Unmapping everything at finalize while the
	// process keeps running would pull that memory out from under the runtime, so the option
	// cannot be honored here; clear it (also reflected back through the effective config).
	global_config.unmap_on_finalize = 0;
#endif

#if PLATFORM_WINDOWS
	SYSTEM_INFO system_info;
	memset(&system_info, 0, sizeof(system_info));
	GetSystemInfo(&system_info);
	os_map_granularity = system_info.dwAllocationGranularity;
	os_virtualalloc2 = 0;
	{
		HMODULE kernelbase = GetModuleHandleW(L"kernelbase.dll");
		if (kernelbase)
			os_virtualalloc2 = (virtualalloc2_fn)(void (*)(void))GetProcAddress(kernelbase, "VirtualAlloc2");
	}
#else
	os_map_granularity = (size_t)sysconf(_SC_PAGESIZE);
#endif

#if PLATFORM_WINDOWS
	os_page_size = system_info.dwPageSize;
#else
	os_page_size = os_map_granularity;
#endif
	os_huge_pages = 0;
	os_thp = 0;
	os_huge_page_shift = 0;

	global_cpu_count = 0;
#if PLATFORM_WINDOWS
	{
		// The processors of every group (ALL_PROCESSOR_GROUPS), a process runs on all of them from Windows 11
		HMODULE kernel32 = GetModuleHandleW(L"kernel32.dll");
		active_processor_count_fn active_processor_count = 0;
		if (kernel32)
			active_processor_count =
			    (active_processor_count_fn)(void (*)(void))GetProcAddress(kernel32, "GetActiveProcessorCount");
		if (active_processor_count)
			global_cpu_count = (uint32_t)active_processor_count(0xFFFF);
	}
	if (!global_cpu_count)
		global_cpu_count = (uint32_t)system_info.dwNumberOfProcessors;
#else
#if defined(__linux__) && defined(SYS_sched_getaffinity)
	{
		// The system call returns the bytes of the affinity mask it copied, the C library wrapper needs _GNU_SOURCE
		unsigned long cpu_mask[1024 / (8 * sizeof(unsigned long))];
		long mask_size = syscall(SYS_sched_getaffinity, 0, sizeof(cpu_mask), cpu_mask);
		for (long iword = 0; iword < (mask_size / (long)sizeof(unsigned long)); ++iword) {
			for (unsigned long word = cpu_mask[iword]; word; word &= word - 1)
				++global_cpu_count;
		}
	}
#endif
	if (!global_cpu_count) {
		long cpu_online = sysconf(_SC_NPROCESSORS_ONLN);
		global_cpu_count = (cpu_online > 0) ? (uint32_t)cpu_online : 0;
	}
#endif
	atomic_store_explicit(&global_page_decommit_delay, PAGE_DECOMMIT_DELAY_MS, memory_order_relaxed);

	rpmalloc_assert(!(global_config.page_size & (global_config.page_size - 1)),
	                "Configured page size must be a power of two");

	// Establish a valid (normal) page size up front so the allocator never observes a
	// zero page size during the rest of initialization. Huge page detection below may
	// raise it to the huge page size once availability has been confirmed.
	if (!memory_interface || (global_config.page_size < os_page_size))
		global_config.page_size = os_page_size;

	if (global_config.enable_huge_pages) {
#if PLATFORM_WINDOWS
		HANDLE token = 0;
#if WINAPI_FAMILY_PARTITION(WINAPI_PARTITION_APP | WINAPI_PARTITION_SYSTEM)
		size_t large_page_minimum = GetLargePageMinimum();
#else
		size_t large_page_minimum = 2 * 1024 * 1024;
#endif
		if (large_page_minimum)
			OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &token);
		if (token) {
			LUID luid;
			if (LookupPrivilegeValue(0, SE_LOCK_MEMORY_NAME, &luid)) {
				TOKEN_PRIVILEGES token_privileges;
				memset(&token_privileges, 0, sizeof(token_privileges));
				token_privileges.PrivilegeCount = 1;
				token_privileges.Privileges[0].Luid = luid;
				token_privileges.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
				if (AdjustTokenPrivileges(token, FALSE, &token_privileges, 0, 0, 0)) {
					if (GetLastError() == ERROR_SUCCESS)
						os_huge_pages = 1;
				}
			}
			CloseHandle(token);
		}
		// Disable huge pages if the system large page size is not a power of two that fits the builtin geometry
		if (os_huge_pages && ((large_page_minimum & (large_page_minimum - 1)) || (large_page_minimum > LARGE_PAGE_SIZE)))
			os_huge_pages = 0;
		if (os_huge_pages) {
			if (large_page_minimum > os_page_size)
				os_page_size = large_page_minimum;
			if (large_page_minimum > os_map_granularity)
				os_map_granularity = large_page_minimum;
		}
#elif defined(__linux__)
		size_t huge_page_size = 0;
		char meminfo[4096];
		if (os_read_system_file("/proc/meminfo", meminfo, sizeof(meminfo))) {
			const char* line = strstr(meminfo, "Hugepagesize:");
			if (line)
				huge_page_size = os_parse_decimal(line + 13) * 1024;
		}
		// Sanity check the detected default huge page size, the builtin page geometry
		// cannot use huge pages larger than the largest builtin page size (e.g. 1GiB
		// default huge pages). Since the huge page size is passed explicitly to mmap,
		// a non-default 2MiB huge page pool can still be used in that case.
		if (huge_page_size & (huge_page_size - 1))
			huge_page_size = 0;
		if (huge_page_size > LARGE_PAGE_SIZE)
			huge_page_size = 2 * 1024 * 1024;
#if defined(MAP_HUGETLB)
		if (huge_page_size) {
			// Hugepagesize in /proc/meminfo only reports the configured huge page size, not
			// whether any huge pages are actually available (the static pool can be empty,
			// HugePages_Total == 0). Probe by mapping a single huge page with the same flags
			// used for real mappings and only enable huge pages if the request can actually
			// be satisfied (static pool or overcommit). This also covers the case where the
			// page size is reported but the kernel rejects that specific MAP_HUGE_* size.
			size_t huge_shift = 0;
			while (((size_t)1 << huge_shift) < huge_page_size)
				++huge_shift;
			int probe_flags = MAP_PRIVATE | MAP_ANONYMOUS | MAP_HUGETLB;
#if defined(MAP_HUGE_SHIFT)
			probe_flags |= (int)(huge_shift << MAP_HUGE_SHIFT);
#endif
			void* probe = mmap(0, huge_page_size, PROT_READ | PROT_WRITE, probe_flags, -1, 0);
			if ((probe != MAP_FAILED) && probe) {
				munmap(probe, huge_page_size);
				os_huge_pages = 1;
				os_huge_page_shift = huge_shift;
				os_page_size = huge_page_size;
				os_map_granularity = huge_page_size;
			}
		}
#endif
#elif defined(__FreeBSD__)
		int rc;
		size_t sz = sizeof(rc);

		if (sysctlbyname("vm.pmap.pg_ps_enabled", &rc, &sz, NULL, 0) == 0 && rc == 1) {
			os_huge_pages = 1;
			os_page_size = 2 * 1024 * 1024;
			os_map_granularity = os_page_size;
		}
#elif defined(__APPLE__)
#if defined(__x86_64__) && !TARGET_OS_IPHONE && !TARGET_OS_SIMULATOR
		// Superpages are only supported on x86-64, Apple silicon has no huge page support
		os_huge_pages = 1;
		os_page_size = 2 * 1024 * 1024;
		os_map_granularity = os_page_size;
#endif
#elif defined(__NetBSD__)
		os_huge_pages = 1;
		os_page_size = 2 * 1024 * 1024;
		os_map_granularity = os_page_size;
#endif
	}

	if (huge_pages_requested && !os_huge_pages) {
		// Explicit huge pages were requested but are not available on this system. Fail
		// initialization rather than silently backing the heap with normal pages behind a
		// huge page size. Leave the allocator uninitialized (and the requested flag cleared)
		// so the caller can detect this and re-initialize without huge pages if desired.
		global_config.enable_huge_pages = 0;
		atomic_store_explicit(&global_rpmalloc_init_state, RPMALLOC_INIT_UNINIT, memory_order_release);
		return -1;
	}

	global_config.enable_huge_pages = os_huge_pages;

	if (os_huge_pages) {
		while (((size_t)1 << os_huge_page_shift) < os_page_size)
			++os_huge_page_shift;
	}

	if (!memory_interface || (global_config.page_size < os_page_size))
		global_config.page_size = os_page_size;

#if PLATFORM_WINDOWS
	// Large pages on Windows must be committed when mapped and can never be decommitted
	if (global_config.enable_huge_pages)
		global_config.disable_decommit = 1;
#endif

	os_huge_reserve = 0;
#if ARCH_64BIT && ENABLE_DECOMMIT
	if (!global_config.disable_decommit && !os_huge_pages && (global_memory_interface->memory_map == os_mmap)) {
		os_huge_reserve = 1;
#if PLATFORM_POSIX
		struct rlimit address_limit;
		if (!getrlimit(RLIMIT_AS, &address_limit) && (address_limit.rlim_cur != RLIM_INFINITY))
			os_huge_reserve = 0;
#endif
#if defined(__linux__) || defined(__ANDROID__)
		char overcommit[16];
		if (os_read_system_file("/proc/sys/vm/overcommit_memory", overcommit, sizeof(overcommit)) &&
		    (overcommit[0] == '2'))
			os_huge_reserve = 0;
#endif
	}
#endif

	int thp_always = 0;
#if defined(__linux__) || defined(__ANDROID__)
	// Transparent huge pages, advise mappings to be huge page backed without requiring
	// a preallocated huge page pool and without affecting page size or decommit
	// The system backs every mapping by huge pages (mode "always"), only mappings advised to be (mode "madvise"),
	// or none (mode "never"). In mode "always" rpmalloc manages the huge pages of its mappings by default, advising
	// the mappings that would make partially used memory resident not to be backed by huge pages. In mode "madvise"
	// it does with enable_thp. disable_thp disables transparent huge pages for the process
	if (!os_huge_pages && !global_config.disable_thp) {
		char thp_state[64];
		int thp_mode = 1;
		if (os_read_system_file("/sys/kernel/mm/transparent_hugepage/enabled", thp_state, sizeof(thp_state)))
			thp_mode = strstr(thp_state, "[never]") ? 0 : (strstr(thp_state, "[always]") ? 2 : 1);
		os_thp_size = 2 * 1024 * 1024;
		if (os_read_system_file("/sys/kernel/mm/transparent_hugepage/hpage_pmd_size", thp_state, sizeof(thp_state))) {
			size_t thp_size = os_parse_decimal(thp_state);
			if (thp_size && !(thp_size & (thp_size - 1)))
				os_thp_size = thp_size;
		}
		// The huge page size can have a mode of its own, and the process can have huge pages disabled
		char thp_path[96];
		snprintf(thp_path, sizeof(thp_path), "/sys/kernel/mm/transparent_hugepage/hugepages-%ukB/enabled",
		         (unsigned int)(os_thp_size / 1024));
		if (os_read_system_file(thp_path, thp_state, sizeof(thp_state)) && !strstr(thp_state, "[inherit]"))
			thp_mode = strstr(thp_state, "[never]") ? 0 : (strstr(thp_state, "[always]") ? 2 : 1);
#if defined(PR_GET_THP_DISABLE)
		if (prctl(PR_GET_THP_DISABLE, 0, 0, 0, 0) > 0)
			thp_mode = 0;
#endif
		thp_always = (thp_mode == 2);
		os_thp = thp_always || ((thp_mode == 1) && global_config.enable_thp);
	}
	if (global_config.disable_thp)
		(void)prctl(PR_SET_THP_DISABLE, 1, 0, 0, 0);
#endif
	global_config.enable_thp = os_thp;
	// Huge block mappings are sized and advised by the huge page size only if it is a multiple of the page size and
	// at most 2MiB. Larger huge pages, as the 32MiB and 512MiB pages of 16KiB and 64KiB page kernels, would round
	// every huge block up to one
	global_thp_mask = 0;
	global_thp_nohuge_mask = 0;
	global_thp_small_page = 0;
	if (os_thp) {
		if ((os_thp_size >= global_config.page_size) && (os_thp_size <= (2 * 1024 * 1024)))
			global_thp_mask |= (1U << PAGE_HUGE);
		if (thp_always)
			global_thp_nohuge_mask = ((1U << PAGE_SMALL) | (1U << PAGE_MEDIUM_SMALL) | (1U << PAGE_MEDIUM_LARGE) |
			                          (1U << PAGE_LARGE)) & ~global_thp_mask;
		size_t thp_page = os_thp_size / SMALL_PAGE_SIZE;
		global_thp_small_page = (thp_page > SPAN_SMALL_FIRST_PAGE) ? (uint32_t)thp_page : SPAN_SMALL_FIRST_PAGE;
	}

	// Decommit of free pages only applies to page types larger than the configured
	// page size, disable the free page overflow pass for page types where it can
	// never apply (always the case when huge pages or a large page size is in use)
	const uint32_t page_free_overflow_default[4] = {16, 8, 4, 2};
	const uint32_t page_free_retain_default[4] = {4, 2, 1, 1};
	const uint32_t builtin_page_size[4] = {SMALL_PAGE_SIZE, MEDIUM_SMALL_PAGE_SIZE, MEDIUM_LARGE_PAGE_SIZE,
	                                       LARGE_PAGE_SIZE};
	for (unsigned int itype = 0; itype < 4; ++itype) {
		int page_can_decommit =
		    !global_config.disable_decommit && ((size_t)builtin_page_size[itype] > global_config.page_size);
		global_page_free_overflow[itype] = page_can_decommit ? page_free_overflow_default[itype] : 0xFFFFFFFFU;
		global_page_free_retain[itype] = page_can_decommit ? page_free_retain_default[itype] : 0xFFFFFFFFU;
	}

#if PLATFORM_WINDOWS
	if (!global_module_detached)
		fls_key = FlsAlloc(&rpmalloc_fls_destructor);
#else
	if (!global_module_detached)
		pthread_key_created = (pthread_key_create(&pthread_key, rpmalloc_thread_destructor) == 0);
#endif

	global_main_thread_id = get_thread_id();
	global_heap_orphan.owner_thread = ORPHAN_HEAP_OWNER;

	atomic_store_explicit(&global_rpmalloc_init_state, RPMALLOC_INIT_DONE, memory_order_release);

	rpmalloc_thread_initialize();

	return 0;
}

extern const rpmalloc_config_t*
rpmalloc_config(void) {
	return &global_config;
}

#if ENABLE_LEAK_DETECTION
//! Number of size class blocks still allocated across every heap. A non-zero value once all threads
//! have finalized means the application has not freed all its allocations (leak or double free).
static long long
statistics_outstanding_blocks(void) {
	heap_t* lists[3] = {global_heap_used, global_heap_queue, global_heap_pristine};
	long long outstanding = 0;
	for (int ilist = 0; ilist < 3; ++ilist) {
		for (heap_t* heap = lists[ilist]; heap; heap = heap->next) {
			for (uint32_t iclass = 0; iclass < SIZE_CLASS_COUNT; ++iclass)
				outstanding += heap->size_use[iclass].alloc_current;
		}
	}
	// The used blocks of the orphan pages are counted on the orphan heap
	for (uint32_t iclass = 0; iclass < SIZE_CLASS_COUNT; ++iclass)
		outstanding += global_heap_orphan.size_use[iclass].alloc_current;
	return outstanding;
}

//! Number of cross-thread freed blocks parked in a span's per page deferred lists, counting every
//! initialized page (a full page is not reachable from page_available but can still hold deferred
//! frees, since adoption is skipped while a page has a local free list). Page count is in the token
//! high bits.
static long long
span_deferred_block_count(span_t* span) {
	long long deferred = 0;
	for (uint32_t ipage = span_first_page(span); ipage < span->page_initialized; ++ipage) {
		page_t* page = span_get_page(span, ipage);
		uint64_t token = atomic_load_explicit(&page->thread_free, memory_order_relaxed);
		deferred += (long long)(uint32_t)((token >> 32ULL) & THREAD_FREE_COUNT_MASK);
	}
	return deferred;
}

//! Number of blocks freed cross-thread but not yet applied to the owning heap's alloc_current,
//! waiting in the deferred free lists. These are accounted in alloc_current until processed, so
//! subtracting them from the outstanding count yields the blocks the application has truly leaked.
static long long
statistics_pending_deferred_blocks(void) {
	heap_t* lists[3] = {global_heap_used, global_heap_queue, global_heap_pristine};
	long long deferred = 0;
	for (int ilist = 0; ilist < 3; ++ilist) {
		for (heap_t* heap = lists[ilist]; heap; heap = heap->next) {
			// Page types small through large carry the per heap full page deferred list and the per
			// page deferred lists of every span. Huge blocks are never deferred per block.
			for (uint32_t itype = 0; itype < 4; ++itype) {
				block_t* block = (block_t*)atomic_load_explicit(&heap->thread_free[itype], memory_order_relaxed);
				while (block) {
					++deferred;
					block = block->next;
				}
				if (heap->span_partial[itype])
					deferred += span_deferred_block_count(heap->span_partial[itype]);
				for (span_t* span = heap->span_used[itype]; span; span = span->next)
					deferred += span_deferred_block_count(span);
			}
		}
	}
	return deferred;
}
#endif

extern void
rpmalloc_finalize(void) {
	if (atomic_load_explicit(&global_rpmalloc_init_state, memory_order_acquire) != RPMALLOC_INIT_DONE)
		return;
	rpmalloc_thread_finalize();

#if HUGE_CACHE_SLOT_COUNT
	// Release cached huge mappings, the cache must not hold memory across a finalize
	// (and potential re-initialize with a different page size)
	huge_cache_flush();
#endif

#if ENABLE_LEAK_DETECTION && ENABLE_ASSERTS
	// Leak detection: every thread has finalized, so blocks still allocated and not merely waiting
	// in a deferred cross-thread free list are blocks the application never freed (or freed twice).
	rpmalloc_assert(statistics_outstanding_blocks() <= statistics_pending_deferred_blocks(),
	                "Memory leak detected");
	rpmalloc_assert(atomic_load_explicit(&global_statistics.huge_alloc, memory_order_relaxed) == 0,
	                "Memory leak detected");
#endif


	if (global_config.unmap_on_finalize) {
		// Heaps can share a mapping (heap_allocate_new); only the owner carries mapped_size != 0.
		// Resources are freed while every heap is still mapped, then owners are unmapped in a
		// separate pass so a heap is never dereferenced after its shared mapping is gone.
		for (uint32_t iclass = 0; iclass < SIZE_CLASS_COUNT; ++iclass)
			atomic_store_explicit(&global_page_orphan[iclass], 0, memory_order_relaxed);
		for (uint32_t itype = 0; itype < 4; ++itype)
			atomic_store_explicit(&global_page_orphan_empty[itype], 0, memory_order_relaxed);
#if ENABLE_STATISTICS
		memset(global_heap_orphan.size_use, 0, sizeof(global_heap_orphan.size_use));
#endif
		heap_t* owners = 0;
		heap_t* heap = global_heap_pristine;
		global_heap_pristine = 0;
		while (heap) {
			heap_t* heap_next = heap->next;
			heap_free_all(heap);
			if (heap->mapped_size) {
				heap->next = owners;
				owners = heap;
			}
			heap = heap_next;
		}
		heap = global_heap_queue;
		global_heap_queue = 0;
		global_heap_orphan_cursor = 0;
		while (heap) {
			heap_t* heap_next = heap->next;
			heap_free_all(heap);
			if (heap->mapped_size) {
				heap->next = owners;
				owners = heap;
			}
			heap = heap_next;
		}
		heap = global_heap_used;
		global_heap_used = 0;
		while (heap) {
			heap_t* heap_next = heap->next;
			heap_free_all(heap);
			if (heap->mapped_size) {
				heap->next = owners;
				owners = heap;
			}
			heap = heap_next;
		}
		while (owners) {
			heap_t* owner_next = owners->next;
			heap_unmap(owners);
			owners = owner_next;
		}
#if ENABLE_STATISTICS
		memset(&global_statistics, 0, sizeof(global_statistics));
#endif
	}

#if PLATFORM_WINDOWS
	if (fls_key != FLS_OUT_OF_INDEXES) {
		FlsFree(fls_key);
		fls_key = FLS_OUT_OF_INDEXES;
	}
#else
	if (pthread_key_created) {
		pthread_key_delete(pthread_key);
		pthread_key_created = 0;
	}
#endif

	global_heap_creating = 0;
	global_main_thread_id = 0;
	atomic_store_explicit(&global_rpmalloc_init_state, RPMALLOC_INIT_UNINIT, memory_order_release);
}

extern void
rpmalloc_thread_initialize(void) {
	if (get_thread_heap() == global_heap_default) {
		if (global_thread.idle_heap)
			heap_idle_attach(global_thread.idle_heap);
		else
			get_thread_heap_allocate();
	}
}

extern void
rpmalloc_thread_idle_begin(void) {
	// The blocks this thread freed into other heaps are pushed, their owners cannot reclaim them otherwise
	thread_free_chain_flush(&global_thread);
	heap_t* heap = get_thread_heap();
	if (!heap || (heap == global_heap_default) || heap->first_class)
		return;
	if (heap->activity != heap->idle_activity) {
		heap->idle_activity = heap->activity;
		heap->idle_ms = monotonic_time_ms();
	}
	global_thread.idle_heap = heap;
	// Only the thread local pointer: the platform key still names the heap, so the thread exit destructor runs and
	// takes it back from the idle state. A free by this thread meanwhile takes the cross-thread path without the
	// free chain, which is safe alongside a reap, and an allocation takes the heap back
	global_thread.heap = global_heap_default;
	atomic_store_explicit(&heap->idle_state, HEAP_STATE_IDLE, memory_order_release);
}

extern void
rpmalloc_thread_idle_end(void) {
	heap_t* heap = global_thread.idle_heap;
	if (heap)
		heap_idle_attach(heap);
}

extern void
rpmalloc_thread_finalize(void) {
	if (global_thread.idle_heap)
		heap_idle_attach(global_thread.idle_heap);
	// Push the blocks this thread freed into other heaps, the thread has no flush point left
	thread_free_chain_release();
#if !defined(__APPLE__)
	global_thread.chain_exit = CHAIN_EXIT_FINALIZED;
#endif
	heap_t* heap = get_thread_heap();
	if (heap != global_heap_default) {
		heap_release_trim(heap);
		set_thread_heap(global_heap_default);
	}
}

//! Hand the blocks of the heap local free list of a size class back to the free lists of their pages, and adopt the
//  blocks other threads freed into the available pages of the size class. Retires the pages left without used blocks
static void
heap_collect_size_class(heap_t* heap, uint32_t size_class) {
	block_t* block = heap->local_free[size_class];
	if (block) {
		// The heap list holds blocks of one page, page_push_local_free_to_heap hands it the page list whole
		heap->local_free[size_class] = 0;
		page_t* page = block_get_page(block);
		block_t* last = block;
		uint32_t count = 1;
		while (last->next) {
			last = last->next;
			++count;
		}
		last->next = page->local_free;
		page->local_free = block;
		page->block_used -= count;
		if (UNEXPECTED(page->is_full != 0))
			page_full_to_available(page);
	}
	page_t* page = heap->page_available[size_class];
	while (page) {
		page_t* next = page->next;
		// A queued page is taken from the pending list, which retires it if it is empty. A free can queue the
		// page while its list is claimed here, then the page is left to the pending list
		unsigned long long token = atomic_load_explicit(&page->thread_free, memory_order_acquire);
		int queued = 0;
		while (token) {
			if (token & THREAD_FREE_QUEUED) {
				queued = 1;
				break;
			}
			if (atomic_compare_exchange_weak_explicit(&page->thread_free, &token, 0, memory_order_acquire,
			                                          memory_order_relaxed))
				break;
		}
		if (queued) {
			page = next;
			continue;
		}
		if (token) {
			block_t* first;
			uint32_t count = page_block_from_thread_free_list(page, token, &first);
			if (count) {
				if (page->local_free) {
					block_t* last = first;
					while (last->next)
						last = last->next;
					last->next = page->local_free;
				}
				page->local_free = first;
				page->remote_fed = 1;
				rpmalloc_assert(count <= page->block_used, "Page thread free list count internal failure");
				page->block_used -= count;
				_rpmalloc_stat_add_free(heap, page->size_class, count);
			}
		}
		if (!page->block_used) {
			// No block of the page is held anywhere, so no other thread can free into it and queue it
			if (heap->page_available[size_class] == page) {
				heap->page_available[size_class] = next;
				if (next)
					next->prev = page->prev;
			} else {
				page_available_unlink(heap, page);
			}
			heap_page_free(heap, page);
		}
		page = next;
	}
}

//! Collect the memory a heap holds without using it: the blocks other threads freed into its pages, the pages
//  left without used blocks, and the free pages beyond the retain count of the small page types
static void
heap_collect_unused(heap_t* heap, uint64_t now) {
#if HUGE_CACHE_SLOT_COUNT
	huge_cache_collect(now);
#endif
	if (atomic_load_explicit(&heap->page_pending, memory_order_relaxed))
		heap_collect_pending_pages(heap);
	heap_collect_thread_free(heap);
	// A heap whose thread stops allocating keeps the free blocks of the pages it last allocated from in its local
	// free lists, and the blocks other threads freed into its available pages in their thread free lists, both
	// counted as used by the pages. A page whose blocks are all in those lists is retired, including the last
	// available page of its size class, which is kept only to spare an allocating thread reinitializing it
	for (uint32_t iclass = 0; iclass < SIZE_CLASS_COUNT; ++iclass)
		heap_collect_size_class(heap, iclass);
	for (uint32_t itype = 0; itype < 4; ++itype) {
		if (global_page_free_overflow[itype] == 0xFFFFFFFFU)
			continue;
		heap->page_free_overflow_ms[itype] = 0;
		// Free medium-large and large pages are all decommitted, a retained page holds the memory of up to the
		// largest block, and the small page types keep their retain count
		uint32_t retain = (itype >= PAGE_MEDIUM_LARGE) ? 0 : global_page_free_retain[itype];
		if (heap->page_free_commit_count[itype] > retain)
			heap_page_free_decommit(heap, itype, retain);
		heap->page_free_low[itype] = heap->page_free_commit_count[itype];
	}
	page_orphan_sweep(now);
}

//! Collect the blocks other threads freed into the full pages of a heap, so the pages they emptied are free, and
//  decommit the free pages held beyond the retain count. Leaves the heap what it keeps for its next allocations
static void
heap_collect(heap_t* heap, uint64_t now) {
#if HUGE_CACHE_SLOT_COUNT
	huge_cache_collect(now);
#endif
	if (atomic_load_explicit(&heap->page_pending, memory_order_relaxed))
		heap_collect_pending_pages(heap);
	heap_collect_thread_free(heap);
	for (uint32_t itype = 0; itype < 4; ++itype) {
		if (global_page_free_overflow[itype] == 0xFFFFFFFFU)
			continue;
		heap->page_free_overflow_ms[itype] = 0;
		if (heap->page_free_commit_count[itype] > global_page_free_retain[itype])
			heap_page_free_decommit(heap, itype, global_page_free_retain[itype]);
	}
}

extern void
rpmalloc_thread_collect(void) {
	// Push the blocks this thread freed into other heaps and collect the blocks other threads freed into full
	// pages, so the pages they emptied are free, then decommit the free pages held above the retain count right
	// away instead of waiting for the decommit delay
	thread_free_chain_flush(&global_thread);
	heap_t* heap = get_thread_heap();
	if (heap == global_heap_default)
		return;
	heap_collect(heap, monotonic_time_ms());
}

extern void
rpmalloc_thread_collect_unused(void) {
	// Push the blocks this thread freed into other heaps, then collect all the heap holds unused right away
	thread_free_chain_flush(&global_thread);
	heap_t* heap = get_thread_heap();
	if (heap == global_heap_default)
		return;
	heap_collect_unused(heap, monotonic_time_ms());
}

#ifndef HEAP_REAP_LIMIT
//! Most idle heaps one rpmalloc_reap_idle_heaps call reaps, the rest wait for the next call
#define HEAP_REAP_LIMIT 64
#endif

//! Check if other threads freed blocks into the available pages of a heap the calling thread holds
static int
heap_has_available_thread_free(heap_t* heap) {
	for (uint32_t iclass = 0; iclass < SIZE_CLASS_COUNT; ++iclass) {
		for (page_t* page = heap->page_available[iclass]; page; page = page->next) {
			unsigned long long token = atomic_load_explicit(&page->thread_free, memory_order_relaxed);
			if ((token >> 32ULL) & THREAD_FREE_COUNT_MASK)
				return 1;
		}
	}
	return 0;
}

//! Check if a heap the calling thread holds was used since its last reap, by its thread or by other threads freeing
//  into its pages
static int
heap_reap_pending(heap_t* heap) {
	return (heap->activity != heap->reaped_activity) || heap_has_thread_free(heap) ||
	       atomic_load_explicit(&heap->page_pending, memory_order_relaxed) || heap_has_available_thread_free(heap);
}

extern size_t
rpmalloc_reap_idle_heaps(unsigned int min_idle_ms, unsigned int max_heaps) {
	thread_free_chain_flush(&global_thread);
	uint64_t now = monotonic_time_ms();
	if ((max_heaps == 0) || (max_heaps > HEAP_REAP_LIMIT))
		max_heaps = HEAP_REAP_LIMIT;
	// The calling thread runs, so its heap is not idle and keeps what it holds for its next allocations. It is
	// collected here if used since its last reap, at most once per min idle time, and is not counted: the thread
	// runs the reaps, and the next reap would always find it used
	heap_t* self = get_thread_heap();
	if (self && (self != global_heap_default) && !self->first_class &&
	    (!self->reap_ms || ((now >= self->reap_ms) && ((now - self->reap_ms) >= min_idle_ms))) &&
	    heap_reap_pending(self)) {
		heap_collect(self, now);
		self->reaped_activity = self->activity;
		self->reap_ms = now;
	}
	// The heaps are claimed, checked and reaped one at a time, a thread taking its heap back waits for the reap of
	// its own heap only. The heap lock is held to walk the used list, not to check or reap a heap, which scans its
	// pages and makes system calls: a claimed heap stays in the list, its thread waits to take it back before it
	// can release it
	size_t count = 0;
	heap_lock_acquire();
	heap_t* heap = global_heap_used;
	while (heap && (count < max_heaps)) {
		// Claimed before its fields are read, its thread writes them while it uses the heap. A heap reaped or found
		// with nothing to reap within the min idle time is skipped before the scan of its pages, a caller reaping in
		// steps walks the heaps again
		unsigned int state = HEAP_STATE_IDLE;
		if ((atomic_load_explicit(&heap->idle_state, memory_order_relaxed) != HEAP_STATE_IDLE) ||
		    !atomic_compare_exchange_strong_explicit(&heap->idle_state, &state, HEAP_STATE_REAPING,
		                                             memory_order_acquire, memory_order_relaxed)) {
			heap = heap->next;
			continue;
		}
		uint64_t since = heap->idle_ms;
		uint64_t reaped = heap->reap_ms;
		int due = (since <= now) && ((now - since) >= min_idle_ms) &&
		          (!reaped || ((reaped <= now) && ((now - reaped) >= min_idle_ms)));
		if (!due) {
			atomic_store_explicit(&heap->idle_state, HEAP_STATE_IDLE, memory_order_release);
			heap = heap->next;
			continue;
		}
		heap_lock_release();
		if (heap_reap_pending(heap)) {
			// The pages the reap retires count as activity of the heap, not of its thread: a thread woken without
			// work keeps its idle time
			heap_collect_unused(heap, now);
			heap->reaped_activity = heap->activity;
			heap->idle_activity = heap->activity;
			++count;
		}
		heap->reap_ms = now;
		heap_lock_acquire();
		heap_t* next = heap->next;
		atomic_store_explicit(&heap->idle_state, HEAP_STATE_IDLE, memory_order_release);
		heap = next;
	}
	heap_lock_release();
	return count;
}

void
rpmalloc_thread_statistics(rpmalloc_thread_statistics_t* stats) {
	memset(stats, 0, sizeof(rpmalloc_thread_statistics_t));
#if ENABLE_STATISTICS
	heap_t* heap = get_thread_heap();
	if (!heap || (heap->id == 0))
		return;

	// Size class cache: blocks available without mapping a new page. A page's block_used counts the
	// blocks held on the heap free list, so those are counted from the heap list and each page adds
	// its remaining blocks (page free list plus the not yet initialized tail).
	for (uint32_t iclass = 0; iclass < SIZE_CLASS_COUNT; ++iclass) {
		size_t block_size = global_size_class[iclass].block_size;
		size_t free_count = 0;
		for (block_t* block = heap->local_free[iclass]; block; block = block->next)
			++free_count;
		for (page_t* page = heap->page_available[iclass]; page; page = page->next)
			free_count += (size_t)(page->block_count - page->block_used);
		stats->sizecache += free_count * block_size;

		stats->size_use[iclass].alloc_current = (size_t)heap->size_use[iclass].alloc_current;
		stats->size_use[iclass].alloc_peak = (size_t)heap->size_use[iclass].alloc_peak;
		stats->size_use[iclass].alloc_total = (size_t)heap->size_use[iclass].alloc_total;
		stats->size_use[iclass].free_total = (size_t)heap->size_use[iclass].free_total;
	}

	// Span cache: free but still committed pages retained per page type
	static const size_t page_type_size[4] = {SMALL_PAGE_SIZE, MEDIUM_SMALL_PAGE_SIZE, MEDIUM_LARGE_PAGE_SIZE,
	                                          LARGE_PAGE_SIZE};
	for (uint32_t itype = 0; itype < 4; ++itype)
		stats->spancache += (size_t)heap->page_free_commit_count[itype] * page_type_size[itype];

	// Per page type span use: current spans in use computed live (partial plus full), map calls counted
	for (uint32_t itype = 0; itype < 5; ++itype) {
		size_t current = 0;
		if ((itype < 4) && heap->span_partial[itype])
			++current;
		for (span_t* span = heap->span_used[itype]; span; span = span->next)
			++current;
		stats->span_use[itype].current = current;
		stats->span_use[itype].map_calls = (size_t)heap->span_map_calls[itype];
	}
#endif
}

void
rpmalloc_dump_statistics(void* file) {
#if ENABLE_STATISTICS
	fprintf(file, "Mapped pages:        %llu\n",
	        (unsigned long long)atomic_load_explicit(&global_statistics.page_mapped, memory_order_relaxed));
	fprintf(file, "Mapped pages (peak): %llu\n",
	        (unsigned long long)atomic_load_explicit(&global_statistics.page_mapped_peak, memory_order_relaxed));
	fprintf(file, "Active pages:        %llu\n",
	        (unsigned long long)atomic_load_explicit(&global_statistics.page_active, memory_order_relaxed));
	fprintf(file, "Active pages (peak): %llu\n",
	        (unsigned long long)atomic_load_explicit(&global_statistics.page_active_peak, memory_order_relaxed));
	fprintf(file, "Pages committed:     %llu\n",
	        (unsigned long long)atomic_load_explicit(&global_statistics.page_commit, memory_order_relaxed));
	fprintf(file, "Pages decommitted:   %llu\n",
	        (unsigned long long)atomic_load_explicit(&global_statistics.page_decommit, memory_order_relaxed));
	fprintf(file, "Huge bytes:          %llu\n",
	        (unsigned long long)atomic_load_explicit(&global_statistics.huge_alloc, memory_order_relaxed));
	fprintf(file, "Huge bytes (peak):   %llu\n",
	        (unsigned long long)atomic_load_explicit(&global_statistics.huge_alloc_peak, memory_order_relaxed));
	fprintf(file, "Heaps in use:        %llu\n",
	        (unsigned long long)atomic_load_explicit(&global_statistics.heap_count, memory_order_relaxed));
#if ENABLE_LEAK_DETECTION
	fprintf(file, "Outstanding blocks:  %lld\n",
	        statistics_outstanding_blocks() - statistics_pending_deferred_blocks());
#endif

	// Aggregate per size class and per page type counters across all in-use heaps. This walks live
	// heaps without stopping the world, so the values are a best-effort snapshot.
	heap_size_use_t size_use[SIZE_CLASS_COUNT];
	memset(size_use, 0, sizeof(size_use));
	uint64_t span_map_calls[5];
	memset(span_map_calls, 0, sizeof(span_map_calls));
	heap_lock_acquire();
	for (heap_t* heap = global_heap_used; heap; heap = heap->next) {
		for (uint32_t iclass = 0; iclass < SIZE_CLASS_COUNT; ++iclass) {
			size_use[iclass].alloc_current += heap->size_use[iclass].alloc_current;
			size_use[iclass].alloc_peak += heap->size_use[iclass].alloc_peak;
			size_use[iclass].alloc_total += heap->size_use[iclass].alloc_total;
			size_use[iclass].free_total += heap->size_use[iclass].free_total;
		}
		for (uint32_t itype = 0; itype < 5; ++itype)
			span_map_calls[itype] += heap->span_map_calls[itype];
	}
	heap_lock_release();

	fprintf(file, "SizeClass  CurAlloc PeakAlloc  TotAlloc   TotFree BlockSize\n");
	for (uint32_t iclass = 0; iclass < SIZE_CLASS_COUNT; ++iclass) {
		if (!size_use[iclass].alloc_total)
			continue;
		fprintf(file, "%9u %9d %9d %9d %9d %9u\n", iclass, size_use[iclass].alloc_current,
		        size_use[iclass].alloc_peak, size_use[iclass].alloc_total, size_use[iclass].free_total,
		        global_size_class[iclass].block_size);
	}
	fprintf(file, "PageType  MapCalls\n");
	for (uint32_t itype = 0; itype < 5; ++itype) {
		if (!span_map_calls[itype])
			continue;
		fprintf(file, "%8u %9llu\n", itype, (unsigned long long)span_map_calls[itype]);
	}
#else
	(void)sizeof(file);
#endif
}

void
rpmalloc_global_statistics(rpmalloc_global_statistics_t* stats) {
#if ENABLE_STATISTICS
    stats->mapped = global_config.page_size * atomic_load_explicit(&global_statistics.page_mapped, memory_order_relaxed);
    stats->mapped_peak = global_config.page_size * atomic_load_explicit(&global_statistics.page_mapped_peak, memory_order_relaxed);
    stats->committed = global_config.page_size * atomic_load_explicit(&global_statistics.page_commit, memory_order_relaxed);
    stats->decommitted = global_config.page_size * atomic_load_explicit(&global_statistics.page_decommit, memory_order_relaxed);
    stats->active = global_config.page_size * atomic_load_explicit(&global_statistics.page_active, memory_order_relaxed);
    stats->active_peak = global_config.page_size * atomic_load_explicit(&global_statistics.page_active_peak, memory_order_relaxed);
    stats->huge_alloc = atomic_load_explicit(&global_statistics.huge_alloc, memory_order_relaxed);
    stats->huge_alloc_peak = atomic_load_explicit(&global_statistics.huge_alloc_peak, memory_order_relaxed);
    stats->heap_count = atomic_load_explicit(&global_statistics.heap_count, memory_order_relaxed);
#else
    memset(stats, 0, sizeof(rpmalloc_global_statistics_t));
#endif
}

#if RPMALLOC_FIRST_CLASS_HEAPS

rpmalloc_heap_t*
rpmalloc_heap_acquire(void) {
	// Must be a pristine heap, or else memory blocks could already be allocated from the heap
	// which would (wrongly) be released when heap is cleared with rpmalloc_heap_free_all().
	// heap_allocate(1) returns either a never-used heap carved from a shared block (whether that
	// block was mapped for an ordinary heap or a prior first-class heap) or a freshly mapped one.
	heap_t* heap = heap_allocate(1);
	rpmalloc_assume(heap != 0);
	heap->owner_thread = 0;
	heap->first_class = 1;
	return heap;
}

//! Stop tracking the huge blocks of a first class heap that is released or becomes the heap of a thread. Any thread
//  can then free them, so the frees must not update the span list, and as for a thread heap rpmalloc_heap_free_all
//  no longer releases them
static void
heap_untrack_huge_spans(heap_t* heap) {
	for (span_t* span = heap->span_used[PAGE_HUGE]; span; span = span->next)
		span_get_huge_page(span)->is_tracked = 0;
	heap->span_used[PAGE_HUGE] = 0;
}

void
rpmalloc_heap_release(rpmalloc_heap_t* heap) {
	if (heap) {
		heap_untrack_huge_spans(heap);
		heap_release_trim(heap);
	}
}

RPMALLOC_ALLOCATOR void*
rpmalloc_heap_alloc(rpmalloc_heap_t* heap, size_t size) {
#if ENABLE_VALIDATE_ARGS
	if (size >= MAX_ALLOC_SIZE) {
		errno = EINVAL;
		return 0;
	}
#endif
	return heap_allocate_block(heap, size, 0);
}

RPMALLOC_ALLOCATOR void*
rpmalloc_heap_aligned_alloc(rpmalloc_heap_t* heap, size_t alignment, size_t size) {
#if ENABLE_VALIDATE_ARGS
	if (size >= MAX_ALLOC_SIZE) {
		errno = EINVAL;
		return 0;
	}
#endif
	return heap_allocate_block_aligned(heap, alignment, size, 0);
}

RPMALLOC_ALLOCATOR void*
rpmalloc_heap_aligned_zalloc(rpmalloc_heap_t* heap, size_t alignment, size_t size) {
#if ENABLE_VALIDATE_ARGS
	if (size >= MAX_ALLOC_SIZE) {
		errno = EINVAL;
		return 0;
	}
#endif
	return heap_allocate_block_aligned(heap, alignment, size, 1);
}

RPMALLOC_ALLOCATOR void*
rpmalloc_heap_calloc(rpmalloc_heap_t* heap, size_t num, size_t size) {
	size_t total;
	if (size_mul_overflow(num, size, &total)) {
		errno = ENOMEM;
		return 0;
	}
#if ENABLE_VALIDATE_ARGS
	if (total >= MAX_ALLOC_SIZE) {
		errno = EINVAL;
		return 0;
	}
#endif
	return heap_allocate_block(heap, total, total);
}

extern inline RPMALLOC_ALLOCATOR void*
rpmalloc_heap_aligned_calloc(rpmalloc_heap_t* heap, size_t alignment, size_t num, size_t size) {
	size_t total;
	if (size_mul_overflow(num, size, &total)) {
		errno = ENOMEM;
		return 0;
	}
#if ENABLE_VALIDATE_ARGS
	if (total >= MAX_ALLOC_SIZE) {
		errno = EINVAL;
		return 0;
	}
#endif
	return heap_allocate_block_aligned(heap, alignment, total, 1);
}

RPMALLOC_ALLOCATOR void*
rpmalloc_heap_realloc(rpmalloc_heap_t* heap, void* ptr, size_t size, unsigned int flags) {
#if ENABLE_VALIDATE_ARGS
	if (size >= MAX_ALLOC_SIZE) {
		errno = EINVAL;
		return ptr;
	}
#endif
	return heap_reallocate_block(heap, ptr, size, 0, flags);
}

RPMALLOC_ALLOCATOR void*
rpmalloc_heap_aligned_realloc(rpmalloc_heap_t* heap, void* ptr, size_t alignment, size_t size, unsigned int flags) {
#if ENABLE_VALIDATE_ARGS
	if ((size + alignment < size) || (alignment > SMALL_PAGE_SIZE)) {
		errno = EINVAL;
		return 0;
	}
#endif
	return heap_reallocate_block_aligned(heap, ptr, alignment, size, 0, flags);
}

void
rpmalloc_heap_free(rpmalloc_heap_t* heap, void* ptr) {
	if (UNEXPECTED(ptr == 0))
		return;
	// A first class heap without an owner thread is owned by the caller. A heap that is the current heap of
	// a thread is owned by that thread only, so it and a null heap compare against the calling thread heap
	block_deallocate_heap(ptr, (heap && !heap->owner_thread) ? heap : 0);
}

//! Free all memory allocated by the heap
void
rpmalloc_heap_free_all(rpmalloc_heap_t* heap) {
	heap_free_all(heap);
}

struct rpmalloc_heap_statistics_t
rpmalloc_heap_statistics(rpmalloc_heap_t* heap) {
#if RPMALLOC_HEAP_STATISTICS
	if (heap) {
		return heap->stats;
	}
#endif
	(void)sizeof(heap);
	struct rpmalloc_heap_statistics_t stats = {0};
	return stats;
}

extern inline void
rpmalloc_heap_thread_set_current(rpmalloc_heap_t* heap) {
	if (global_thread.idle_heap)
		heap_idle_attach(global_thread.idle_heap);
	heap_t* prev_heap = get_thread_heap();
	if (prev_heap != heap) {
		// The new heap can be null, which leaves the thread without flush points
		thread_free_chain_release();
		if (heap)
			heap_untrack_huge_spans(heap);
		set_thread_heap(heap);
		if (prev_heap)
			heap_release(prev_heap);
	}
}

rpmalloc_heap_t*
rpmalloc_get_heap_for_ptr(void* ptr) {
	span_t* span = (span_t*)((uintptr_t)ptr & SPAN_MASK);
	if (span)
		return block_get_page(ptr)->heap;
	return 0;
}

#endif

#include "malloc.c"
