// Counts glibc allocator calls in a process and reports them on exit.
//
// Interposes the four entry points Rust's `std::alloc::System` uses on Unix and
// forwards to glibc's own symbols, which avoids the dlsym-during-init recursion a
// RTLD_NEXT lookup would risk. Counters are relaxed: the report is a total, and no
// reader observes them before exit.
#define _GNU_SOURCE
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

extern void *__libc_malloc(size_t);
extern void *__libc_calloc(size_t, size_t);
extern void *__libc_realloc(void *, size_t);
extern void __libc_free(void *);

static atomic_ullong n_malloc, n_calloc, n_realloc, n_free, n_memalign;

void *malloc(size_t size) {
    atomic_fetch_add_explicit(&n_malloc, 1, memory_order_relaxed);
    return __libc_malloc(size);
}

void *calloc(size_t count, size_t size) {
    atomic_fetch_add_explicit(&n_calloc, 1, memory_order_relaxed);
    return __libc_calloc(count, size);
}

void *realloc(void *ptr, size_t size) {
    atomic_fetch_add_explicit(&n_realloc, 1, memory_order_relaxed);
    return __libc_realloc(ptr, size);
}

void free(void *ptr) {
    if (ptr) atomic_fetch_add_explicit(&n_free, 1, memory_order_relaxed);
    __libc_free(ptr);
}

int posix_memalign(void **out, size_t align, size_t size) {
    atomic_fetch_add_explicit(&n_memalign, 1, memory_order_relaxed);
    // glibc has no __libc_posix_memalign; over-aligned blocks are rare here and
    // aligned_alloc is not interposed, so it reaches the real allocator.
    void *p = aligned_alloc(align, size);
    if (!p) return 12;
    *out = p;
    return 0;
}

// The running total, for a caller scoping a measurement to one phase of its own run.
unsigned long long allocount_total(void) {
    return atomic_load_explicit(&n_malloc, memory_order_relaxed)
         + atomic_load_explicit(&n_calloc, memory_order_relaxed)
         + atomic_load_explicit(&n_realloc, memory_order_relaxed)
         + atomic_load_explicit(&n_memalign, memory_order_relaxed);
}

__attribute__((destructor)) static void report(void) {
    const char *path = getenv("LORE_ALLOC_COUNT_FILE");
    FILE *out = path ? fopen(path, "a") : stderr;
    if (!out) return;
    fprintf(out, "allocount pid=%d malloc=%llu calloc=%llu realloc=%llu memalign=%llu free=%llu total=%llu\n",
            (int)getpid(),
            atomic_load(&n_malloc), atomic_load(&n_calloc), atomic_load(&n_realloc),
            atomic_load(&n_memalign), atomic_load(&n_free),
            atomic_load(&n_malloc) + atomic_load(&n_calloc) + atomic_load(&n_realloc) + atomic_load(&n_memalign));
    if (path) fclose(out);
}

__attribute__((constructor)) static void announce(void) {
    if (getenv("LORE_ALLOC_COUNT_DEBUG")) fprintf(stderr, "allocount loaded pid=%d\n", (int)getpid());
}
