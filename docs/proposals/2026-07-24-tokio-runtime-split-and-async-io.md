---
lep: 2026-07-24-tokio-runtime-split-and-async-io
title: A Bounded Thread Model — Network Runtime Split, Inline Compute, and a Dedicated File I/O Engine
authors:
  - mattias.jansson
status: Draft
created: 2026-07-24
updated: 2026-07-24
discussion: <TBD — fill in CR link when discussion CR is opened>
---

# A Bounded Thread Model — Network Runtime Split, Inline Compute, and a Dedicated File I/O Engine

## Summary

This proposal replaces Lore's open-ended thread model with a fixed budget known at initialization, through three structural moves. First, the network transport (QUIC, gRPC) moves onto a small dedicated runtime — two threads on the client; up to one per processor on the server, where thousands of concurrent client connections are the normal case — so protocol timers, TLS, and packet processing are never delayed by compute or I/O continuations saturating the core workers. Second, the separate compute thread pool is deleted: compression, hashing, and chunking are fragment-sized work units (≤256 KiB, single-digit milliseconds worst case) that run inline in tasks on the per-processor core workers, removing a full CPU-count thread population. Third, file I/O leaves the tokio blocking pool for a dedicated I/O engine: a bounded, idle-reaped pool of positional-syscall threads (`min(2 × cores, 32)`) as the portable baseline. The positional syscall is the permanent engine on macOS and the fallback wherever completion-based I/O is unavailable and upgraded to completion-based asynchronous I/O (io_uring on Linux, overlapped I/O on Windows) where the platform offers it. With async I/O the data-plane parallelism (reads, writes, flushes) becomes operating-system queue depth instead of thread count, with a limited syscall pool retained for metadata and composite operations. What remains of the core runtime's blocking pool is a residual ~4 threads serving OS APIs with no asynchronous form.

Today a Lore process may run three overlapping thread populations — tokio async workers, up to 128 tokio blocking threads, and a rayon CPU-count-sized compute pool — with the network sharing all of them and file I/O parallelism capped by blocking-thread availability. After this change the process runs per-processor core workers, a small fixed net runtime, the bounded I/O engine (the capped, idle-reaped syscall pool, plus one completion reaper where the platform provides completion-based I/O), and the residual blocking threads — every population a fixed cap known at initialization. The CLI, the C API, the wire protocol, and all on-disk formats are unchanged.

## Motivation

Lore's throughput on an end-user machine is limited by its thread model, not by the hardware.

**Three thread populations oversubscribe the machine and make the footprint unpredictable.** The blocking pool is sized `min(2 × (processor_count + 1), 128)` and the compute pool one per core less one (`lore-base/src/runtime.rs`); together with the per-core async workers, a 32-core workstation runs roughly 130 threads that fluctuate with load. Under a write-heavy burst the workers and the compute pool are simultaneously CPU-hungry — roughly two runnable threads per core contending for cycles — converting CPU quota into context-switch overhead instead of work. Thread count also inflates memory independently of load: Lore links a thread-caching allocator (rpmalloc, the `#[global_allocator]` in `lore-base/src/allocator`), which keeps a per-thread cache of spans, so every thread carries resident memory beyond its stack, and a hundred-plus fluctuating threads amplify the process's steady-state footprint well past the cost of the stacks alone. Lore also ships as `lore-capi` for embedding into host applications — editors, build systems, asset pipelines. A revision-control library that spawns over a hundred threads inside a host that is itself using every core (shader compilation, lighting builds, asset cooking) degrades the host and makes Lore's own performance unpredictable. An embeddable library should have a footprint the host can reason about: a fixed number of threads, known at initialization. Better still, the host should be able to cap that footprint with a single knob — one "use at most this many threads" limit for the whole library — but today's three independently formula-sized populations make that impractical: there is no single quantity to set, and no clean way to translate one budget into three separately derived counts. Offering that knob should be a goal, and this proposal makes it reachable — once every population is a fixed cap rather than a load-driven formula, a single thread-budget maximum can be apportioned across the core workers, the net runtime, the I/O engine, and the residual pool by a straightforward calculation.

**The network shares runtime with core logic.** QUIC ack processing, flow control, retransmission timers, TLS, and HTTP/2 framing run on the same workers that execute CPU intensive logic and file I/O continuations. Under heavy CPU load (tree traversal, hashing, inline compression during commit, inline decompression during sync), network transfers can stall because protocol processing competes with compute for the same threads — latency-sensitive timer work queued behind millisecond-scale polls.

**File I/O parallelism is proportional to thread count, and thread count is wrong in both directions.** With few exceptions, every file operation in Lore today is either a `tokio::fs` call or a `spawn_blocking` closure, and both execute the actual syscall on the tokio blocking thread pool. On a 4-core laptop the sizing formula yields 10 threads — so a clone materializing tens of thousands of fragments, or a status walk over a 10,000-file workspace, can have at most 10 file operations in flight, while a modern NVMe drive needs queue depths in the dozens to hundreds to deliver its rated throughput. The machines where this ceiling binds hardest are common developer machines: laptops, small CI runners, and containers with CPU quotas. On a 32-core workstation the same formula yields 66 blocking threads that contribute to the oversubscription above without delivering proportional I/O parallelism.

**Every file operation pays a round trip through another thread.** A `tokio::fs::read` is: enqueue a closure to the blocking pool, wake a blocking thread, run the syscall, wake the originating task back on an async worker. Two cross-thread handoffs per operation, plus thread churn from the pool's 10-second keep-alive under bursty load. For workspace-scan workloads — at least one `stat`/`open`/`read` per file during `status`, `stage`, and `commit` — the handoffs are a per-file tax on latency and on cache locality. Linux (io_uring, kernel 5.6+) and Windows (overlapped I/O on completion ports) both support submitting file I/O asynchronously and receiving completions without dedicating a thread to each operation; Lore uses neither.

**What this costs users today:**

- Inside a host application, Lore runs up to four threads per processor — roughly 130 on a 32-core workstation — and the count fluctuates with load, so the host cannot budget for it.
- Under heavy CPU load, network transfers stall because protocol processing competes with logic and compute for the same thread pool.
- On low-core machines, `clone`, `sync`, `status`, and `stage` are throttled by a ten-thread pool while the disk and the network sit underused — the machines with the least headroom lose the most.

None of this ceiling comes from the data path. Content is stored as fragments of 32–256 KiB (`lore-storage/src/fragment_engine.rs`), and the data pipelines (clone, commit, defragment) are bounded producer/consumer task graphs that already issue far more concurrent operations than the pools can carry. The thread model alone is the bottleneck.

## Goals / Non-Goals

### Goals

1. **Bound the process to a fixed thread budget known at initialization, controllable through a single limit.** No blocking pool sized by formula, no separate compute pool; beyond the per-processor core workers, the only additions are the network runtime (two threads on the client; up to core count on the server), the bounded file I/O engine (an idle-reaped syscall pool of `min(2 × cores, 32)`, plus a single completion reaper thread on completion-capable platforms), and a residual pool (≤4 threads) for genuinely blocking OS APIs. Because every population is a fixed cap rather than a load-driven formula, the host can set one "use at most this many threads" budget and have the library apportion it across the core workers, the net runtime, the I/O engine, and the residual pool by a straightforward calculation — the single knob an embeddable library should offer.
2. **Isolate network transport from CPU saturation.** QUIC and gRPC processing runs on a dedicated runtime so that ack processing, flow control, and retransmission timers are not delayed by compression work occupying the core workers.
3. **Run CPU-bound work (compression, hashing, chunking) on all cores without a separate pool.** Fragment-sized work units execute inline in tasks on the core runtime.
4. **Make file I/O parallelism independent of thread count wherever the platform offers completion-based I/O.** In-flight file operations are bounded by queue depth and the existing fragment memory budget, not by available threads.
5. **Eliminate the blocking-thread round trip for data-plane file I/O on completion-capable platforms.** A read, write, or flush is submitted from the worker that runs the task and completes via a waker, with no handoff to a dedicated syscall thread; metadata and composite operations keep one pool dispatch.
6. **Preserve every external contract.** CLI behavior, `lore-capi` entry points and callback semantics, wire protocol, and on-disk formats are unchanged; the portable pooled backend preserves exact current I/O semantics on platforms or environments where completion-based I/O is unavailable.

### Non-Goals

- Replacing tokio as the async runtime. Both runtime domains remain tokio; this proposal changes thread topology and the file I/O mechanism, not the task model. A custom per-core executor remains a possible future step behind the same internal facade (see Alternatives).
- Bypassing the page cache (`O_DIRECT`, `FILE_FLAG_NO_BUFFERING`) or changing durability semantics. Reads and writes keep their current caching and `fsync` behavior.
- Changing `lore-server`'s request-handling architecture. The server gains the same runtime-topology and storage-layer improvements and sheds the same pools, but its axum/tonic/quinn service stack is out of scope.
- Adding new public API surface. No new CLI verbs, capi functions, or protocol messages.

## Proposed Design

The design lands in two phases. Phase 1 splits the network transport onto a dedicated runtime and folds CPU work inline onto the core workers (Goals 1–3, 6) — a contained change to runtime topology with no file I/O dependency. Phase 2 introduces the file I/O engine and shrinks the blocking pool to its residual (Goals 1, 4–6). The ordering inside phase 1 is load-bearing: compute moves inline only once the network has its own runtime, because inline compute deliberately saturates the core workers and would otherwise starve protocol timers. The phases are independent of each other: the driver's futures are pure waker-based completions with no scheduler coupling, so phase 2 runs identically under the split topology, and phase 1 does not depend on how file I/O is issued.

### Phase 1 — dedicated net runtime and inline CPU work

**Topology.** The shared runtime (`lore-base/src/runtime.rs`) splits into a *core runtime* — `worker_threads = processor_count`, which is already tokio's default — and a *net runtime* with a fixed thread count. Both are sized through `TokioSettings` (and `LORE_NET_THREADS`) from the first release. The client defaults to 2 net threads — one user's worth of QUIC streams and gRPC channels. The server defaults to one net thread per processor: serving thousands of concurrent client connections is its normal case, and TLS, QUIC packet processing, and HTTP/2 framing for that fan-in warrant the same core-count parallelism as compute. Per-runtime scheduler metrics extending the existing instrumentation (`lore-server/src/telemetry/tokio_bridge.rs`) validate both defaults during rollout. Nearly every spawn in the workspace already routes through the `lore_spawn!` macro family, which resolves to a single `runtime()` accessor, so the split is one new accessor (`net_runtime()`) plus a `lore_spawn_net!` macro variant with identical context-propagation semantics. The handful of direct spawn call sites (the event relay in `lore-revision/src/relay.rs`, scattered `tokio::spawn` uses in `lore-server`, `lore`, and `lore-revision`) migrate to the macros in this phase, and clippy `disallowed-methods` fences keep direct spawning out thereafter. Under `#[tokio::test]`, the test's own runtime stands in for the core runtime (as the `Handle::try_current()` path already provides today), and the net runtime is created lazily on first use, exactly as the shared runtime is now.

**What lives on the net runtime.** Tokio I/O resources and the internal driver tasks of quinn and tonic bind to the runtime where they are constructed, but their request futures are waker-based and can be awaited from any runtime. Tracing the transport layer shows the per-request paths (`Storage::get` over QUIC and gRPC) are exactly that — semaphore acquire, channel send, oneshot await — with no timers or spawns in the hot path. The complete set that must be constructed or spawned on the net runtime is small: QUIC endpoint creation (`lore-transport/src/quic/client.rs`), tonic channel construction (`lore-transport/src/grpc/mod.rs`) and the per-exchange auth-service channel (`lore-transport/src/auth/ucs_auth.rs`), the connection-setup task tree (`lore-transport/src/connection.rs`), the auth-refresh and reconnect loops, the per-session gRPC stream multiplexers, and the notification event loop (`lore-notification/src/client.rs`). Everything else — every call site holding an `Arc<dyn Storage>` or other service trait — continues to await those calls directly from core-runtime tasks. A core task that misses in the local store simply awaits the remote fetch; the worker thread runs other tasks for the duration, and the existing in-flight deduplication and connection-level concurrency limits carry over unchanged. No blocking work belongs on the net runtime; its blocking pool is pinned to a single thread so a stray `spawn_blocking` cannot grow it.

**CPU work inline.** With the network isolated, this phase deletes the rayon compute pool (`lore-base/src/runtime.rs`), and its dispatch sites — FastCDC chunking (`lore-storage/src/fragment_engine.rs`) and compression (`lore-storage/src/compress/pool.rs`) — become plain inline code in async tasks. Work units are fragment-sized (≤256 KiB; single-digit milliseconds worst case through zstd, sub-millisecond through blake3), which is fine-grained enough for fair scheduling without explicit yield points. The existing lock-free scratch-buffer and zstd-context pools transfer as-is, re-sized from compute-pool thread count to worker count. Two server-side `block_in_place` hazards must be fixed in this phase because per-core workers make them deadlock-prone: the JWT interceptor blocking on every gRPC request (`lore-server/src/auth/jwt_interceptor.rs`) becomes genuinely async, and the replication client's blocking `Drop` (`lore-server/src/quic/replication_store_service/client.rs`) becomes an explicit async close. The remaining `block_in_place` sites (AWS plugin initialization, transport shutdown, telemetry setup) are startup- or shutdown-scoped and move to the residual pool in phase 2.

**The blocking pool is untouched in this phase.** File I/O still runs on it at today's sizing; it shrinks to its residual only when phase 2 gives file I/O its own engine. Phase 1's thread reduction comes entirely from deleting the compute pool — one full CPU-count population — and its isolation win from the net split.

### Phase 2 — the file I/O engine

The engine ships as a dedicated `lore-io` crate: three backends behind one trait, the buffer pool, and a conformance suite that runs identically against every backend. `lore-base` keeps only the runtime accessors and spawn macros.

**API shape.** Completion-based I/O cannot borrow caller memory — the kernel owns the buffer while an operation is in flight — so the driver exposes an owned-buffer, positional API:

```rust
let file = io::open(path, OpenOptions::read()).await?;
let (buf, n) = file.read_at(buf, offset).await?;   // buffer travels with the op
let (buf, n) = file.write_at(buf, offset).await?;
file.sync_data().await?;
```

Buffers come from a size-classed pool aligned with the storage layer's fragment sizes (32–256 KiB classes plus a large scratch class). The pool uses stable backing allocations so io_uring fixed-buffer registration (`IORING_REGISTER_BUFFERS`) can be adopted later as a pure optimization, subject to the `RLIMIT_MEMLOCK` constraint noted under Security Considerations. Pooled buffers convert into the `Bytes` values the storage API already traffics in via `Bytes::from_owner` (available since bytes 1.9), so reads flow into existing interfaces without copying. The operation set is open, positional read/write, fsync/fdatasync, close, stat, fallocate, rename, unlink, and mkdir, plus composite whole-file operations — `write_file_bytes(path, Bytes) -> Metadata` (create, write, optionally fdatasync, stat) and `read_file_bytes(path) -> Bytes` (open, stat, read, close) — matching the atomic whole-file patterns used throughout the storage layer (`lore-storage/src/read.rs`) and keeping small-file scans at one dispatch per file.

**Three backends behind one trait — a pooled baseline and two completion upgrades.**

- *psync* (portable baseline): the trait implemented with positional syscalls (`pread`/`pwrite`) executed on a dedicated bounded syscall pool — `min(2 × cores, 32)` threads, idle-reaped. Pooled rather than inline execution is deliberate: a syscall against a slow filesystem (cold media, NFS) blocks for milliseconds or indefinitely, and inline execution would let a handful of such operations freeze the entire core runtime, while pool threads — like today's blocking pool — absorb the wait. Seastar's dedicated syscall thread is the same answer to the same problem. This backend is not a mere fallback: it is the permanent engine on macOS, which offers no completion-based file I/O — kqueue does not cover regular-file data operations, and POSIX AIO and `dispatch_io` are thread pools underneath, which is why libuv and tokio also run file I/O on a pool there. On Linux it is selected when io_uring is unavailable, which is common, not exotic: Docker's default seccomp profile has blocked io_uring syscalls since 2023 (moby/moby#46762), and kernels older than 5.6 lack it entirely. Even at its cap the pool is a quarter of today's blocking-pool ceiling, and it is dedicated: file I/O no longer competes with anything else for its threads. The backend is probed once per process at startup; `LORE_IO_BACKEND` and a per-repository override (`io.backend` in the repository's `.lore/config.toml`) force a backend for diagnosis or unusual filesystems. There is no automatic per-mount detection: io_uring remains correct on network and FUSE filesystems (operations complete via io-wq), so backend choice never affects correctness, only performance. Flush-class operations are ordinary operations on the syscall pool.
- *io_uring* (Linux 5.6+): a single shared submission ring guarded by a sub-microsecond submit lock — submission-side critical sections are ~100 ns of SQE preparation, far below contention at fragment-sized operation rates. The data plane — reads, writes, fsync/fdatasync — submits as SQEs; the control plane (open, stat, rename, unlink, mkdir, and the composite operations) stays on the shared syscall pool. That split is deliberate, not provisional caution: metadata operations are not async-native inside the kernel — a ring-submitted `openat` or `statx` is punted to a kernel io-wq worker making the same blocking call, so ring submission buys no parallelism there, only SQE/CQE overhead, and some operations (directory enumeration) have no uring opcode at all. Linked SQE chains (`IOSQE_IO_LINK`, with open-into-fixed-slot) can later move the composites onto the rings as a pure optimization. Buffered data-plane operations that miss the page cache complete via the kernel's io-wq workers; those are kernel-managed, bounded (`IORING_REGISTER_IOWQ_MAX_WORKERS`), and carry no user-space stacks, so the thread-count goals — scheduling, memory, embedding footprint — are unaffected.
- *Overlapped I/O* (Windows): files open with `FILE_FLAG_OVERLAPPED` and associate with one shared I/O completion port; reads and writes submit as overlapped operations. Operations with no overlapped form — the metadata control plane (`CreateFile`, rename, preallocation via `SetFileInformationByHandle`) and flushes (`FlushFileBuffers`, milliseconds to tens of milliseconds against the device) — run on the shared syscall pool, mirroring the uring control-plane split. Whether the NT-level flush (`NtFlushBuffersFileEx`) can pend through the completion port on overlapped handles is worth a spike during implementation; if so, flushes become ordinary overlapped operations and join the data plane.

**Completion delivery.** A single dedicated reaper thread parks directly in `io_uring_enter(GETEVENTS)` / `GetQueuedCompletionStatusEx`, drains completions in batches, and calls each operation's `Waker`. A reaper thread rather than reactor-integrated polling keeps the driver independent of any runtime: the same driver works under the production multi-thread runtime, under the single-threaded runtimes that the ~680 `#[tokio::test]` tests create, and identically on either side of the phase 1 topology split. If profiling ever justifies sharding to multiple rings, the reaper switches to waiting on per-ring eventfds and nothing else changes. The psync backend needs no reaper — its syscall-pool threads complete operations and call wakers directly. The thread arithmetic on completion-capable platforms is: remove up to 128 blocking threads, add one reaper, and keep the bounded syscall pool for the control plane — where, with the data plane on the rings, it sits idle-reaped near zero outside metadata bursts.

**Cancellation and buffer safety.** An in-flight operation owns its buffer until the kernel reports completion; cancelling a task issues an async cancel and recycles the buffer only after the corresponding completion arrives. This is structural in the owned-buffer API — there is no path that frees kernel-visible memory early.

**What migrates.** A survey of the file I/O sites across `lore-storage`, `lore-revision`, and `lore-base` found 34 distinct sites, none of which leak file types across public API boundaries, so the migration is entirely internal. Most are mechanical: the pack store already uses positional I/O (`FileExt::read_exact_at`/`write_all_at` on Unix, `seek_read`/`seek_write` on Windows — `lore-storage/src/packstore.rs`) and maps one-to-one onto `read_at`/`write_at`. Three sites need structural decisions, each an improvement in its own right. The defragment data path (`lore-storage/src/defragment.rs`) consolidates entirely onto the driver: the mutex-plus-seek file sink becomes plain concurrent `write_at` to disjoint offsets, and the migration removes the parallel memory-mapped read and write variants rather than porting them — deleting a dual-path sink and the page-fault stalls that mmap hides from the scheduler. Bucket deserialization (`lore-storage/src/local/immutable_store.rs`) replaces three position-dependent sequential reads with one positional read of the bucket followed by in-memory parsing. The whole-file read-then-hash path (`lore-storage/src/write.rs`) becomes a chunked `read_at` loop feeding an incremental hasher — 1 MiB chunks from the scratch class, double-buffered so the read for the next chunk is in flight while the current chunk hashes — which bounds hashing throughput by the slower of disk rate and hash rate instead of their sum, and removes the largest single-poll stall in the codebase (hashing a multi-gigabyte file in one call). File locking (`lore-base/src/fs/lock.rs`) converts from blocking `flock` with thread-sleep retries to `LOCK_NB` with async retry. Directory enumeration stays as inline syscalls on the workers — io_uring has no getdents operation, and page-cached directory walks are microsecond-scale.

**The residual blocking pool.** With file I/O on its own engine, the core runtime's blocking pool shrinks to a fixed cap of ~4 threads serving genuinely blocking OS APIs with no async form: OS keyring access (`lore-credential`), AWS SDK initialization on the server, and service IPC pipe reads. The cap is core-count-independent by design — nothing that scales with load runs there anymore. CLI-process concerns such as spawning a pager sit outside the library thread model and are handled by the CLI itself, out of scope here. The migration sweep also fixes several pre-existing unwrapped blocking calls currently running on runtime threads (TLS certificate loading in `lore-transport/src/tls.rs`; synchronous file and keyring access in `lore-credential/src/token_store.rs`).

**Thread budget.**

| | async workers | blocking pool | compute pool | net runtime | I/O engine |
|---|---|---|---|---|---|
| today | cores | up to 128 | cores − 1 | — | — |
| after phase 1 | cores | up to 128 | — | 2 (client) / cores (server) | — |
| after phase 2 | cores | ~4 | — | 2 (client) / cores (server) | ≤ `min(2 × cores, 32)` pooled, +1 reaper on completion backends |

The I/O engine's syscall pool exists on every backend — on completion-capable platforms it carries the control plane (open, stat, rename, composites), on psync it carries everything — capped at a quarter of today's blocking-pool ceiling and idle-reaped to near zero when the workload is not I/O-bound. The completion backends add one reaper thread on top. The platform difference is not the thread budget but where data-plane parallelism comes from: queue depth on the rings versus pool threads.

**Goal tracing.** Goal 1 → thread budget table; Goal 2 → net runtime topology; Goal 3 → inline CPU work; Goal 4 → driver queue depth + buffer pool, bounded by the existing fragment memory budget (`lore-storage/src/concurrency.rs`); Goal 5 → submission from the calling worker, waker-based completion; Goal 6 → psync backend, unchanged macro semantics, unchanged capi plumbing.

## Compatibility

- **Wire format** — N/A: no serialization, framing, or compression changes.
- **Client/server protocols** — N/A: no new or changed RPCs; the transport stack is rehosted onto a dedicated runtime but speaks the identical protocol.
- **On-disk format** — No format changes. The driver preserves the existing atomic temp-file-plus-rename patterns, file locking, and `durable`-flag fsync behavior; a repository written under the new I/O layer is indistinguishable from one written today.
- **CLI and public API** — No syntax, exit-code, output, or `lore-capi` surface changes. Capi callbacks continue to fire on a library-owned thread; the specific thread is unspecified today and remains unspecified.
- **Configuration and environment variables** — `LORE_COMPUTE_THREADS` becomes inert in phase 1 and `LORE_BLOCKING_THREADS` in phase 2 (each accepted with a deprecation warning) since the pools they size no longer exist. New in phase 1: `LORE_NET_THREADS` for the net runtime, with matching fields in the server's `tokio` settings section. New in phase 2: `LORE_IO_BACKEND` (`auto` | `uring` | `iocp` | `psync`) plus a per-repository override (`io.backend` in `.lore/config.toml`) for diagnosis and rollback. `LORE_WORKER_THREADS` keeps its meaning (core runtime sizing).

## Non-Functional Considerations

- **Concurrency** — Semantics are preserved: the same locks, atomic-rename patterns, in-flight write deduplication, and fragment memory budget govern concurrent operations. Effective I/O concurrency rises from blocking-pool size to queue depth bounded by the memory budget on completion-capable platforms (the psync syscall pool keeps it thread-bounded, on a dedicated pool no longer shared with anything else); the defragment sink additionally loses a serializing mutex in favor of disjoint-offset concurrent writes.
- **Latency** — Bounded jitter is the trade for inline CPU work: tokio polls its timer and I/O drivers between task polls, so a core runtime saturated with fragment-sized work (sub-millisecond to single-digit-millisecond polls) delays timers by at most tens of milliseconds — acceptable for progress ticks and retry backoff. The latency-sensitive timers (QUIC loss recovery, keep-alive) live on the net runtime and are unaffected by core saturation — which is exactly why the net split precedes inline compute.
- **Memory** — Improves on both axes: up to ~130 thread stacks are replaced by a fixed thread set, and the new buffer pool is explicitly size-classed and bounded by the existing 1 GiB fragment memory budget rather than implicit per-thread scratch allocations. Streaming behavior is unchanged; nothing new buffers proportionally to repository size (the whole-bucket deserialization read is bounded by existing bucket size limits).
- **Statelessness** — Unchanged in kind: the net runtime and the driver (rings/port, buffer pool, reaper thread) are lazily initialized process-level state with the same lifecycle as today's shared runtime, and are drained and torn down by the existing shutdown path (`runtime_shutdown_timeout`; core first, then net, since guarded core tasks may still flush writes over the network).
- **Determinism** — Unchanged. Completion order is already unordered today (the blocking pool completes out of order); all order-sensitive writes already sequence through explicit awaits, and hashing/compression outputs do not depend on scheduling.

## Migration Plan

N/A — no breaking changes, no user-facing migration. Internal delivery is phased as described in Proposed Design: phase 1 (net runtime + inline compute) lands as one contained slice; phase 2 is sliced per subsystem (pack store, local stores, defragment, fragment engine, revision file operations, blocking-pool shrink), each landed green against the full test suite, with the psync backend and `LORE_IO_BACKEND=psync` serving as a per-process kill switch that restores today's syscall behavior without a rollback of the code.

## Security Considerations

This proposal does not change the trust model, the protocol, or what data is read or written — only where threads run and how file syscalls are issued. Two points deserve note. First, io_uring is a recurring kernel attack surface and is consequently disabled in many sandboxed environments (Docker's default seccomp profile, gVisor); the design treats this as a first-class deployment reality via the startup probe and psync fallback rather than requiring elevated privileges or profile changes. Second, if fixed-buffer registration (`IORING_REGISTER_BUFFERS`) is adopted as an optimization, registered memory counts against `RLIMIT_MEMLOCK`; the driver must degrade to unregistered buffers when the limit is low rather than failing.

## Privacy Considerations

No privacy implications: no new data, identifiers, paths, or metadata become visible to any party. The change is confined to thread topology and the I/O mechanism within the process; telemetry gains only aggregate scheduler/queue metrics of the same kind already emitted for the tokio runtime, and the ability to delete or expire repository data is untouched.

## Risks and Assumptions

**Assumptions**

- **Assumption:** quinn/tonic request futures remain awaitable from a foreign runtime (waker-only hot paths, internal driver tasks pinned at construction). — *invalidated if:* a transport dependency upgrade introduces `Handle::current()` or timer creation inside per-request poll paths, which would force per-request gateway hops (spawn + oneshot) at the service-trait boundary.
- **Assumption:** fragment-sized CPU work units (≤256 KiB) are short enough for fair inline scheduling without yield engineering. — *invalidated if:* profiling shows multi-tens-of-millisecond single polls on real data, which would require chunked codec calls or a yield discipline.
- **Assumption:** completion-based I/O outperforms the pooled psync backend for Lore's fragment-sized operations on cold and warm caches alike. — *invalidated if:* benchmarks on target hardware show parity; the thread-count and embedding benefits stand on their own, but the throughput motivation for the completion backends would need to be re-scoped.

**Risks**

- **Risk:** network code silently spawned or constructed on the core runtime after phase 1, reintroducing jitter. — *mitigation:* clippy `disallowed-methods` fences on direct spawn/construct APIs outside the sanctioned modules, plus per-runtime metrics distinguishing the two domains.
- **Risk:** inline compute saturating workers exposes latent blocking calls on the core runtime (the `block_in_place` class). — *mitigation:* the two known hazards are fixed in phase 1; the migration sweep and clippy fences flush out the rest.
- **Risk:** regressions across the 34-site I/O migration. — *mitigation:* per-subsystem slices, each landed green against the full test suite; the psync backend implements identical semantics to today's syscalls, isolating "new driver" from "new backend" failures.
- **Risk:** buffer lifetime bugs around cancellation of in-flight kernel operations (use-after-free class). — *mitigation:* the ownership-passing API makes early frees unrepresentable; cancellation paths await the kernel completion before recycling, and the driver is fuzzed with forced-cancellation tests.
- **Risk:** Windows overlapped semantics differ subtly (handles must be opened overlapped; some operations complete synchronously). — *mitigation:* the driver owns all file opens, and the IOCP backend is developed against the same conformance test suite as the uring and psync backends.
- **Risk:** the psync syscall pool default (`min(2 × cores, 32)`) under-provisions slow network filesystems relative to today's pool (up to 128 threads). — *mitigation:* the pool size is configurable, queue-depth telemetry validates the default during rollout, and macOS — as a permanent psync platform — is benchmarked explicitly rather than treated as a fallback case.

## Drawbacks

- Lore takes ownership of a platform I/O driver — kernel-interface code previously delegated to std and tokio.
- The owned-buffer positional API is more verbose than `&mut [u8]` borrowing at every migrated call site.
- Two runtimes complicate the mental model and double the scheduler metrics surface.
- On platforms without completion-based file I/O (macOS; locked-down containers and old kernels on Linux) the syscall pool is the whole engine: a smaller, fixed, dedicated thread budget compared with today, but no queue-depth parallelism for the data plane.

## Alternatives Considered

### Status quo with tuned pool sizes

Raise `LORE_BLOCKING_THREADS` defaults and size the compute pool more aggressively.

*Rejected because:* parallelism stays proportional to thread count, which is the disease — more threads on a 4-core machine is oversubscription, not throughput, and the per-operation cross-thread round trip remains.

### Net runtime split and inline compute without the I/O engine

Stop after phase 1: delete the compute pool, isolate the network, keep file I/O on the blocking pool.

*Rejected as an end state because:* it fixes oversubscription and network jitter but leaves both file I/O problems — parallelism capped by a thread-count formula and the per-operation round trip — and the embedding budget still contains an "up to 128 threads" line item that the host cannot reason about. It is, however, exactly the right first slice, which is why it is phase 1.

### Replace the runtime wholesale with a custom per-core executor

One worker per core, each owning an io_uring ring/IOCP association, with tasks parked directly on completion queues — no tokio in the core at all.

*Rejected because:* it forfeits tokio's hardening and instrumentation, requires migrating ~680 tests and reworking the capi entry plumbing and task-local context propagation, and its unique benefits over this proposal (completion-driven parking, task priorities, core pinning) are latency-control features, not throughput features. The internal facade introduced by this proposal deliberately keeps that path open as a future engine swap, decided on measurement.

### Adopt an existing completion-based runtime (glommio, monoio, compio, tokio-uring)

*Rejected because:* glommio and tokio-uring are Linux-only (and tokio-uring is coupled to a single-threaded runtime model); monoio's Windows support does not cover overlapped file I/O; compio is the closest fit but is built around thread-per-core `!Send` tasks without work stealing, while Lore's codebase is structured around `Send` tasks and `JoinSet` fan-out throughout.

## Prior Art

- **DataFusion / InfluxDB IOx** publicly document the dual-tokio-runtime pattern — CPU-bound work inline on one runtime, network I/O on another — as their recommended architecture, which is the phase 1 topology here.
- **libuv (Node.js)** implements asynchronous file I/O as a blocking thread pool — precisely the model phase 2 retires, and a well-documented source of the same throughput ceiling (`UV_THREADPOOL_SIZE`).
- **Seastar / ScyllaDB** demonstrates the end state of completion-based per-core I/O with bounded threads, at the cost of a bespoke framework; this proposal borrows the I/O model while keeping a mainstream runtime.
- **Git** parallelizes workspace scans with short-lived thread pools (e.g. preload-index); it shows the same many-small-files pressure but no async I/O answer worth borrowing.

## Unresolved Questions

- Validate the shipped net-runtime defaults (client 2, server one per processor) against per-runtime scheduler metrics during the phase 1 rollout; both remain configurable, and the defaults are adjusted if real-world data disagrees.
