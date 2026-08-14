# File I/O engine

The `lore-io` crate is Lore's file I/O engine: positional, owned-buffer file operations whose futures suspend on wakers alone and are therefore independent of the runtime driving them. It is the intended replacement for every `std::fs` and `tokio::fs` call inside the library. `lore-storage` is the only crate depending on it so far, so beyond that crate's paths it is exercised by its own conformance suite and benchmark example.

Measurements quoted here are summaries. `lore-io/BENCHMARKS.md` holds the results, the protocol, and the experiments behind them.

## Driver and backend selection

`IoDriver` is the entry point. It holds an `Arc<DriverInner>`, an enum with one variant per backend, and every operation dispatches through a match on that enum rather than through a trait object. Cloning an `IoDriver` increments a reference count, and clones share the backend.

| Backend | Selector | Implementation |
| --- | --- | --- |
| `psync` | `BackendKind::Psync` | Positional syscalls on the shared syscall pool. `pread`/`pwrite` through `FileExt::read_at`/`write_at` on Unix; `ReadFile`/`WriteFile` with the crate's own `OVERLAPPED` on Windows. |
| `uring` | `BackendKind::Uring` | Linux only. Positional read, write and sync submitted to sharded `io_uring` instances; everything else forwards to `psync`. |
| `iocp` | `BackendKind::Iocp` | Windows only. Positional read and write issued as overlapped operations against one I/O completion port; everything else forwards to `psync`. |
| `auto` | `BackendKind::Auto` | Selects `psync` on every platform. |

`auto` selects `psync` everywhere, and the completion backends are opt-in. The two measurements disagree on that choice. Synthetic phases favour the completion backends: on Linux the ring measures 1.60× the syscall pool for commit-shaped reads on ext4, and on Windows `iocp` reaches 1.76× and 4.80× on the synthetic read phases while running two threads against a saturated thirty-two. The smoke suite, driving the real call sites end to end, measures a regression under them and recovers under `psync`. A whole-workload result outranks a per-operation one, so `psync` is the default.

That leaves an open question rather than a closed one. The two measurements disagree, and where the end-to-end cost sits has not been profiled — the candidates include the submission path, the completion handoff, and the control-plane operations that forward to the pool on both completion backends. `BackendKind::Uring`, `BackendKind::Iocp` and `LORE_IO_BACKEND` select them explicitly, so that investigation needs no code change to run its A/B.

The behavioural selection criterion sketched for Linux — submit one read, observe whether it completed during the submit syscall, and choose per machine — remains the shape a future automatic rule would take, and it classifies tmpfs correctly without enumerating filesystems. It is only worth building once a completion backend wins the end-to-end measurement.

`IoDriver::from_env` reads `LORE_IO_BACKEND`, accepting `auto`, `psync`, the empty string, `uring` on Linux and `iocp` on Windows; any other value is an `InvalidInput` error naming the accepted set. A backend absent from the build is rejected rather than silently falling back, so a value that selects a ring on Linux cannot mean something else on macOS. `IoDriver::global()` initializes one driver per process from the environment on first use; an unrecognized value is reported and the probed backend is used instead, because the variable exists for diagnosis and rollback and a typo in it must not fail a host application's first file read. `backend_name()` returns the resolved backend for logs and benchmark labels.

## Windows handles are overlapped

Handles the driver opens carry `FILE_FLAG_OVERLAPPED`, and the data path issues `ReadFile`/`WriteFile` through `overlapped.rs` rather than `std`'s `seek_read`/`seek_write`. Both halves are required.

A handle opened without the flag is a synchronous file object, and the I/O manager serializes every operation on it. That contradicts the property this API is built on, a shared handle carrying concurrent operations at disjoint offsets, and the serialization measures 2× warm and 3× cold. `pread` on a shared descriptor takes no equivalent lock, so no Unix platform pays it.

`std`'s positional calls cannot be used on such a handle. They place the offset in an `OVERLAPPED`, pass no event, and report `ERROR_IO_PENDING` as an ordinary error. That status means the kernel has accepted the request and still owns the buffer, so a caller treating it as a failure frees memory the kernel is about to write into. Each operation therefore owns its completion: its own `OVERLAPPED`, a per-thread manual-reset event, and a `GetOverlappedResult` wait that a pending operation cannot return past. A null event signals the file handle instead, which two concurrent operations on one handle cannot share. A failed wait cancels and waits again rather than returning while the kernel can still touch the buffer.

The wait blocks, which is the purpose of the syscall pool. It runs on every operation: a buffered read on an overlapped handle reports `ERROR_IO_PENDING` in every case measured, at every thread count. Each positional operation therefore parks a pool thread until the kernel completes it, and the pool cap bounds how many can be in flight. Removing that bound is what the `iocp` backend does.

Two further consequences. Overlapped handles maintain no file cursor, so `positional_reads_have_no_cursor` in the conformance suite pins a property of the handle as well as of the API. And these operations work on synchronous handles, which they must, because the whole-file composites open their own.

## Share modes are the call site's

`CreateFileW` opens exclusive by default. `std` passes `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE` on every handle, so a Rust program on Windows behaves like a Unix one, and the driver's default does the same. Exclusion is opt-in, stated by the call site that needs it through `OpenOptions::share_mode`.

That direction is the one the platform's rule makes safe. Windows checks sharing both ways: an open fails when the access it requests is outside an existing handle's share mode, and equally when its own share mode omits a right an existing handle was already granted. The second half is the one that catches you out, because it fails opens that have nothing to do with what the file contains — a read-only open declaring only `FILE_SHARE_READ | FILE_SHARE_DELETE` is refused whenever anything else holds the file with write access. Most files Lore opens are files it does not own: working-tree files an editor, a build tool or the engine holds at the same moment. A narrow default therefore turns an ordinary stage, commit or reset into a failure over a handle Lore has no say in, and — since `ERROR_SHARING_VIOLATION` is what the transient classifier exists for — only after the full retry budget. Sharing by default and excluding deliberately puts the platform-specific failure where someone chose it.

The pack store is where it is chosen. A packfile is the store's own and no other process may write one while Lore holds it open, so both opens in `packstore.rs` name `FILE_SHARE_READ`. The race a strict reader would have prevented elsewhere is caught rather than prevented, by reads that ask for an exact length and fail short — `a_file_shorter_than_its_measured_size_fails` pins that, and it is the mechanism the chunker's exact reads exist to provide.

The whole-file composites build their options through `to_std_blocking`, which carries the share modes without the overlapped flag those synchronous handles cannot use.

One ceiling remains. Concurrent reads against a single file plateau near 91k ops/s on Windows regardless of handle or thread count, while the same reads across distinct files scale to 477k. That is a cache-manager property no backend can avoid, and it bounds any single-file workload on the platform. The storage layer's reads are not of that shape: addresses are hash-distributed over 256 pack-file groups whose files roll at 3 GiB, so a server holds thousands of files and concurrent reads scatter across them. The ceiling is specific to Windows; on macOS, spreading the same reads over one file per thread raises throughput rather than lifting a plateau.

## Buffer ownership

Completion-based backends hand the kernel a pointer into a buffer and keep it there while the operation is in flight, so the API cannot borrow caller memory. The two directions resolve that differently, because the memory has different origins.

**Writes take the caller's buffer by value** and return it on completion. `StableBuf` marks the types whose backing memory does not move for the value's lifetime; `Bytes` and `Vec<u8>` implement it. The data belongs to the caller, so there is nothing to allocate.

**Reads allocate their own buffer, inside the submitted job, and return `Bytes`.** No caller buffer is involved, which satisfies the completion contract directly: the job owns the memory until the kernel is done with it. Two consequences follow:

- The allocation happens on the thread that receives the kernel's copy. The process allocator is rpmalloc (`lore-base/src/allocator`), which caches spans per thread, so buffer reuse is thread-local without this crate managing it.
- `Bytes` is the type the storage layer traffics in, so nothing is copied at the boundary. `BytesMut::freeze` makes the conversion free.

Read buffers are allocated uninitialised through `buffer::uninit_buffer` and truncated to the byte count before becoming `Bytes`, so no uninitialised byte is observable. Zeroing first would write every byte twice, approximately a tenth of the benchmark's CPU time.

There is no buffer pool. The process allocator's per-thread span cache already provides the locality one would supply.

The capability this shape does not offer is reading into memory the caller already owns — filling a window in place. Nothing requires it; a chunker port that does would need the API extended.

## File handle operations

`IoFile` pairs an `Arc<std::fs::File>` with the driver that opened it. Cloning shares the handle. There is no file cursor: every operation is positional, and concurrent operations on one handle at disjoint offsets are safe and unordered.

| Operation | Behavior |
| --- | --- |
| `read_at(max_len, offset)` | Reads up to `max_len` bytes, returning what arrived. Fewer than `max_len` means the file ended; empty means the offset was already at or past the end. |
| `read_exact_at(len, offset)` | Loops until `len` bytes are read. `UnexpectedEof` if the file ends first. |
| `write_at(buffer, offset)` | Writes the buffer contents. Returns the buffer and the byte count. |
| `write_all_at(buffer, offset)` | Loops until the whole buffer is written. `WriteZero` if the file stops accepting bytes. |
| `sync_data()` / `sync_all()` | `fdatasync` / `fsync`. |
| `metadata()`, `set_len(len)` | Handle-scoped `stat`, and resize. Extending leaves a hole that reads as zeros. |

Reads take their length as an argument and writes take theirs from the buffer: a read has no length to infer until the caller states how much it wants, and a write already holds the bytes.

On `psync` the looping forms complete in a single backend dispatch, because the loop runs inside the submitted job, which already owns the buffer and the thread. A short read there costs another syscall rather than another round trip through the pool. On the completion backends each pass is its own submission, because the buffer must travel into the operation entry for the kernel to write into it.

There is no preallocating operation, which is a deliberate departure from the operation set the proposal lists. `posix_fallocate` reserves blocks only on some Linux filesystems — on ZFS it reserves nothing, and is indistinguishable from `set_len` — so an operation named for reservation would guarantee different things on different mounts and no test could pin it portably. Nothing in the workspace requires reservation: the one site that sizes a file up front, the defragment output in `lore-storage/src/defragment.rs`, does so to obtain a hole that reads as zeros. A caller that needs blocks committed should receive an operation named for that guarantee.

Path-scoped operations live on the driver: `open`, `metadata`, `rename`, `copy`, `remove_file`, `create_dir_all`, `remove_dir`, `remove_dir_all`, `set_permissions`, and `read_dir`. `OpenOptions` mirrors the `std::fs` builder for `read`, `write`, `create`, `create_new`, and `truncate`.

`read_dir` returns a `DirStream` rather than a list. A listing is one `getdents` plus a stat per child, and the stats are most of it; the stream resolves a chunk of entries per dispatch and holds one chunk, so a directory of any width costs the same memory and a consumer that stops early stops the walk. Entries carry the metadata of what a name refers to, following links, or `None` where that could not be resolved — a link with no target, or a name unlinked between the listing and the stat. An entry the walk could not read is yielded as an error rather than skipped, because skipping is indistinguishable from the name not being there, and a scan deciding what changed cannot afford that confusion where a listing can.

Every operation dispatches through the backend, including the metadata ones. A completion backend keeps those on the syscall pool regardless, because a ring-submitted `statx` is punted to a kernel worker making the same blocking call. Routing them anyway allows a backend to override any operation, makes a driver instance self-contained rather than partly bypassed, and leaves the syscall pool reachable only from the backends.

## Whole-file composite operations

Two driver operations complete a whole file in a single backend dispatch, matching the atomic whole-file patterns the storage layer uses and keeping small-file scans at one dispatch per file rather than separate open, stat, and read round trips.

- `read_file_bytes(path)` — open, stat, read to the stat length, close. Returns `Bytes` without copying. A file that shrinks mid-read fails with `UnexpectedEof`.
- `write_file_bytes(path, data, durable)` — create or truncate, write, `fdatasync` when `durable` is set, stat, close. Returns the resulting `Metadata`.

Their callers are the shapes they were built for: `write_file_bytes` writes an unfragmented fragment's content in the materialization path, and `read_file_bytes` reads a fan-out level marker, a file that is one 16-byte header.

Both are bounded by `WHOLE_FILE_LIMIT`, 8 MiB, and fail with `InvalidInput` above it. Each holds a pool thread and the whole file resident for its duration, so a large file would occupy one of at most `min(2 × cores, 16)` threads for the transfer; such a caller wants `open` with `read_exact_at` or `write_all_at`. The write's size check runs before the open, so a rejected call cannot have truncated an existing file.

## Syscall pool

`SyscallPool` is a bounded pool of threads dedicated to blocking syscalls, independent of any tokio pool. One process-wide instance backs every operation on the `psync` backend and every control-plane operation on `uring` and `iocp`.

| Property | Value |
| --- | --- |
| Maximum threads | `min(2 × cores, 16)`, from `available_parallelism` (2 when unavailable) |
| Spawn policy | On demand: a submission spawns a thread only when no thread is idle and the cap is not reached |
| Idle policy | A thread exits after 10 seconds without work |
| Thread names | `lore-io-<n>` |
| Queue | Unbounded `VecDeque`, drained FIFO |

Pooled rather than inline execution is deliberate: a syscall against cold media or a network filesystem blocks for milliseconds or indefinitely, and running it inline on an async worker would allow a small number of such operations to stall the runtime. The cap is an eighth of the tokio blocking pool's 128-thread ceiling, and the pool is dedicated, so file I/O does not compete with unrelated blocking work for threads.

`submit` returns a `SyscallTask<T>`, which resolves through an ordinary waker and therefore runs under any executor — the multi-thread runtime in production, and the single-threaded runtimes `#[tokio::test]` creates. Work is wrapped in `catch_unwind`, so a panic on a pool thread resumes on the awaiting task rather than terminating the worker.

`pool_stats()` returns a `PoolStats` snapshot: queued, executing and live threads, the thread and queue high-water marks, and the cap. It is taken under the pool's own lock, so the counts are consistent rather than independently sampled, and the marks are maintained inside the critical section the submission path already enters. The high-water figures are what makes the cap checkable against a real workload, since a queue running deep alongside idle threads indicates the cap is not the limiting factor.

The pool size is fixed at first use. `LORE_IO_POOL_THREADS` overrides the formula, rejecting zero and anything above 128 — the ceiling of the pool this engine replaces, so the override cannot size the engine above it — and reporting an unusable value before falling back to the formula. The override is for measurement and rollback; sizing for a deployment belongs to the `ThreadCounts` apportionment in `lore-base/src/runtime.rs`, which the engine joins when the blocking pool shrinks to its residual.

Three findings shape the formula:

- No single cap is optimal. Warm work favours fewer threads and cold work more, and the two can pull in opposite directions on one machine.
- The ceiling of 16 costs single digits against 32 on every phase measured, and the threads are the return, since this pool shares a process-wide budget.
- Falling below the workload's concurrency costs sharply — cold reads measure 0.54× at 8 threads and 0.65× at 4 against workloads offering 64 and 128 in flight. That ratio is pool size against in-flight requests rather than against core count. The formula reaches those sizes on machines of four cores and fewer and has no floor to prevent it.

The core count includes cores that compute slowly. A thread parked in `pread` waits on a device rather than occupying a core, so a slow core still carries a request. Cached reads are core-bound and peak near the performance-core count on Apple silicon, but sizing this pool to that peak costs a fifth of the device's cold read throughput. Core speed governs the `ThreadCounts` worker sizing instead.

## Runtime independence

The library depends on no async runtime. tokio is a dev-dependency, for the tests' own runtime. Nothing calls `Handle::current()`, spawns a task, or creates a timer. Operations are submitted from whatever thread polls the future, and completion arrives through a waker.

Submitted work and the slot its result is published into share one allocation: `Task<T, F>` in `pool.rs`, held by the queue as a `Job` to run and by the future as a `Completion` to read. The completion backends follow the same shape, with one entry per operation carrying the buffer, the file handle and the result slot.

`operations_complete_under_a_foreign_executor` in the conformance suite drives reads, writes, whole-file operations and a cancellation under `futures::executor::block_on` with no tokio runtime present. Two consequences matter for the migration: the engine behaves identically on either side of the core/net runtime split, and a completion backend slots in behind the same API by delivering wakeups from a reaper thread rather than from a pool thread, with no change to callers.

## Replacing `std::fs` and `tokio::fs`

A file operation in Lore is currently either a `tokio::fs` call or a `lore_spawn_blocking!` closure. Both execute the syscall on the tokio blocking pool, sized `min(2 × (cores + 1), 128)`, which costs two cross-thread handoffs per operation and sets a parallelism ceiling from a thread-count formula — 10 threads on a four-core machine, where a clone materializing tens of thousands of fragments needs far more requests in flight to keep the device busy.

The proposal's survey found 34 distinct file I/O sites across `lore-storage`, `lore-revision`, and `lore-base`, none of which leak file types across a public API boundary. The migration is therefore entirely internal, and lands per subsystem — pack store, local stores, defragment, fragment engine, revision file operations — each slice green against the full test suite before the next.

Most sites are mechanical: the pack store already uses positional I/O and maps one-to-one onto `read_at` and `write_at`. Three required a structural decision, and all three have landed:

- **Defragment data path** — the file sink issues concurrent positional writes to disjoint offsets on one shared `IoFile`, with no memory-mapped variant, so there is one sink and no page-fault stalls hidden from the scheduler. On Windows its handle is overlapped and carries its own `OVERLAPPED`, so those writes overlap as they do on Unix; the synchronous handle `seek_write` requires would have the kernel serialize them. The output is sized up front with `set_len`, and the sink's byte-count check turns an uncovered range from a silent zero-filled hole into a failure.
- **Bucket deserialization** — the position-dependent sequential reads became one `open_read_head` that returns the handle, the size and the head bytes together, with a vectored scatter straight into the bucket's `GrowVec` chunks for anything the head does not cover.
- **Whole-file read-then-hash** — a whole-file mapping feeding a single hash call became `lore-storage/src/chunker.rs`, which streams windows and cuts content-defined boundaries identical to running `FastCDC` over the whole file. Its reads are `IoFile::read_exact_vectored_at` scatters into the window they fill, so the window travels into the operation and comes back with it rather than being read into by a blocking call.

File locking changed mechanism rather than structure: blocking `flock` with thread-sleep retries became `LOCK_NB` with async retry, so no lock acquisition occupies a pool thread.

Directory enumeration is a driver operation. A listing is not one syscall but `getdents` plus a stat per child, so running it inline puts a walk's worth of blocking calls on an async worker — the dirty scan does this per directory level across a whole working tree. `read_dir` forwards to the syscall pool on both completion backends, as the other control-plane operations do, `io_uring` having no `getdents`. The stat it resolves per entry is work its callers need, so the listing carries it rather than leaving each caller to ask again.

Two constraints bound the replacement. Operations with no asynchronous form — OS keyring access, AWS SDK initialization, service IPC pipe reads — stay on a residual blocking pool of approximately four threads, core-count-independent because nothing that scales with load runs there. And `std::fs` remains correct in tests, build scripts, and CLI-process code outside the library thread model; the target is the library's own I/O paths.

Progress is observable as a shrinking match count. These are raw line counts over `lore-*/src` and `lore/src`, including imports and test modules, so they overstate distinct call sites and serve as a trend line:

| Crate | `tokio::fs` | `std::fs::` |
| --- | --- | --- |
| `lore-revision` | 7 | 26 |
| `lore-storage` | — | 51 |
| `lore-server` | 2 | 30 |
| `lore` | 3 | — |
| `lore-base` | 1 | 13 |

`lore-storage` and `lore-revision` are migrated; most of what their counts still show is test modules. The library calls that remain in `lore-storage` are the directory walks, the two `Drop` implementations that remove a temporary file where nothing can be awaited, the `exists` probes the store open paths take before creating a directory, the bucket-version probe in `local/immutable_store.rs`, and the permission and directory-sync helpers in `fs_util.rs`. In `lore-revision` they are `Metadata` in type position, the ProjFS provider, and `sync_dir`; its `tokio::fs` count is entirely fixture setup in `repository/clone.rs`'s test module.

`lore-revision/clippy.toml` fences the `tokio::fs` methods the driver has an answer for, the same mechanism that excludes direct `tokio::spawn`. `canonicalize`, `hard_link`, `read_link` and `symlink_metadata` are not fenced, the driver offering nothing to use instead. `lore-storage` has no crate-level `clippy.toml` and so inherits the root one, which carries no filesystem entries.

## The `psync` backend

`psync` is the permanent engine on macOS, which offers no completion-based file I/O, and the fallback wherever completion-based I/O is unavailable. On Linux that is a common case rather than an exotic one, since Docker's default `seccomp` profile blocks `io_uring` syscalls and kernels older than 5.6 lack them. The permanence is measured: on macOS `psync` reaches the device's cold read ceiling, matching plain threads issuing the same `pread` at equal concurrency, and matches or exceeds `spawn_blocking` on every warm phase.

It is also the semantic reference. Every other backend is conformance-tested against the behaviour defined here.

## The `uring` backend

`uring` splits the operation surface. Positional read, write and sync are submitted to the ring; open, stat, directory operations and the whole-file composites forward to `PsyncDriver` and run on the syscall pool, because a ring-submitted `openat` or `statx` is punted to a kernel worker making the same blocking call and buys no parallelism over the pool. They remain defined on the backend rather than omitted, so that linked-SQE chains could replace them without the dispatch changing.

Three structural properties:

**Sharded rings, one per core, capped at 32.** The kernel executes synchronously-completing operations under a per-ring lock during submit, so a single ring serializes every page-cache copy regardless of how many threads submit. Where the filesystem completes a cached read during the submit syscall, the submitting thread performs the copy, so the shard count bounds how much copying proceeds in parallel. A thread stays on the shard it first used, keeping its submissions and inline drains on one ring.

**Inline drain of synchronous completions.** A page-cache hit is already complete when the submit syscall returns, so the submitting thread drains the completion queue immediately after its enter and resolves the future with no cross-thread round trip.

**A reaper polling the ring file descriptors.** Anything still pending is completed by one reaper thread polling the ring descriptors, which report `POLLIN` while a completion queue is non-empty. That is level-triggered and independent of how the kernel executed the operation, where eventfd notification misses task-work completions. Wakes reach the awaiting executor through plain wakers, so the futures stay runtime-independent.

Buffer ownership is what makes cancellation sound, and it follows the contract the rest of the crate uses: the operation entry owns the buffer and the file handle for the kernel's whole view of the operation. Dropping the future marks the entry abandoned rather than freeing anything, and whichever thread drains the completion releases the payload once the kernel is finished with it. The completion future returns the payload on every result, errors included, so a caller can resubmit the same buffer, which is what allows the read and write loops to retry `EINTR` as `psync` does.

Ring throughput depends on whether the filesystem completes a cached read during the submit syscall. ext4 implements `IOCB_NOWAIT` and completes inline, so no io-wq worker appears and the submitting thread performs the copy. ZFS does not, so every read is punted to io-wq — and io-wq grows its pool only when a worker blocks, which a cache hit never does, so a small number of workers perform all the copying.

The measured consequence is that the ring's advantage is workload- and filesystem-dependent. It measures up to 6.5× on synthetic phases where completion is inline, 1.60× on commit-shaped reads on ext4, and 0.70× to 0.90× on the remaining repository-shaped cases. Scattered writes are its weakest workload at every size and on both filesystems measured, while it wins the sequential write phase on ext4. `lore-io/BENCHMARKS.md` has the figures and the sweeps.

## The `iocp` backend

`iocp` uses the same data-plane split: positional read and write are issued as overlapped operations against one I/O completion port, and open, stat, sync, directory operations and the whole-file composites forward to `PsyncDriver`. Here the split is a platform constraint rather than a staging decision, because Windows has no overlapped `FlushFileBuffers`, `CreateFile` or metadata call; those block whoever issues them, so a pool thread is the appropriate place for them.

The backend shares its submission machinery with `overlapped.rs` — the handles, the per-operation `OVERLAPPED`, and the kernel-owned buffers. What differs is what happens after the submission: a wait on a pool thread there, a packet on a port here.

Because a positional operation on this platform always defers, `psync` parks a pool thread across every operation and its cap bounds how many can be in flight. `iocp` holds no thread across an operation: submission costs one syscall on the calling thread, and the completion arrives as a packet. It measures 1.76× and 4.80× on the two synthetic read phases while running two threads — one reaper, and one pool thread serving the open — against a syscall pool saturated at thirty-two with a queue up to 99 deep. For a library sharing a process-wide thread budget with a host application, the thread count is as material as the throughput.

Four structural properties:

**Submission on the calling thread.** `ReadFile` returns `ERROR_IO_PENDING` without waiting for the device, so issuing an operation does not block the executor thread that polls the future.

**One port and one reaper.** A completion port has no per-instance submission lock, so there is nothing for sharding to relieve. The reaper count is one because the drain is batched: `GetQueuedCompletionStatusEx` takes up to 64 packets per syscall, and threads sharing a port divide the arriving packets between them, so each wakes for a partial batch and the syscall count rises with the thread count while the work does not. `LORE_IO_IOCP_REAPERS` overrides the count for measurement.

**Inline completions are skipped, which is a correctness requirement.** `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS` prevents an operation the kernel finishes during the issuing call from also queueing a packet. Without it the submitting thread would complete the operation and the reaper would complete it again, freeing the entry twice. An open that cannot set the mode fails rather than proceeding. `FILE_SKIP_SET_EVENT_ON_HANDLE` accompanies it: nothing here waits on the file handle's event, and leaving the I/O manager to signal it would make the handle a write-shared cache line between the concurrent operations this API exists to allow.

**One allocation per operation and no type erasure.** The `OVERLAPPED` the kernel writes into lives at offset zero of the operation entry, so the pointer returned by the port is the entry, and a `repr(C)` prefix carries the completion function monomorphised for that entry's payload type. Buffer ownership follows the contract above: the entry owns the buffer and the file handle for the kernel's whole view of the operation, and an abandoned future marks the entry rather than freeing anything.

The backend's concurrency is its caller's task count, because nothing else holds an operation in flight. Throughput therefore rises with the number of tasks driving it up to approximately sixteen and declines past that, where `psync` is flat because its parallelism comes from the pool. That is the source of the 0.72× on the commit-shaped read phase, which drives one task per core; at sixteen tasks the same code measures 1.6× `psync` on the same shape. Reducing that sensitivity is outstanding work, and it is why `psync` remains selectable by name on the platform.

Cold, the two backends tie on the 64 KiB phase — both saturate a SATA SSD at 488 MiB/s, which is the expected result and a check that the warm figures are not an artefact of the harness — and `iocp` takes the 4 KiB phase by 2.27×, over ranges that do not overlap. That phase moves a third of the drive's sequential rate, so it is bound by requests in flight rather than by bytes, which is the same bound the warm result reports at a larger margin. Whole-file reads are a null in both cases, because both backends forward them to the syscall pool.

## Source pointers

- `lore-io/src/driver.rs::IoDriver`, `::BackendKind` — backend selection and operation dispatch.
- `lore-io/src/psync.rs::PsyncDriver` — the positional-syscall backend and the platform `read_at`/`write_at` shims.
- `lore-io/src/uring.rs::UringDriver` — the `io_uring` backend: shard selection, submission, inline drain, and the reaper loop.
- `lore-io/src/overlapped.rs` — the Windows positional operations, the per-thread completion event, and why `std`'s shims are unusable against an overlapped handle.
- `lore-io/src/iocp.rs::IocpDriver` — the completion-port backend: handle registration, submission, the entry layout the port round-trips through, and the reaper loop.
- `lore-io/src/pool.rs::SyscallPool`, `::SyscallTask`, `::default_max_threads` — the bounded pool and its runtime-independent completion future.
- `lore-io/src/buffer.rs::StableBuf`, `::uninit_buffer` — the write-side buffer contract, and the uninitialised allocation reads fill.
- `lore-io/src/file.rs::IoFile`, `::OpenOptions` — the positional handle operations.
- `lore-io/src/dir.rs::DirStream`, `::DirEntry` — the chunked listing and what an entry describes.
- `lore-io/tests/conformance.rs` — the semantic reference every backend must satisfy; new backends join the `drivers` list.
- `lore-io/examples/bench.rs` — the comparison against `spawn_blocking` and against `tokio::fs`, one engine per process. Results, protocol and experiments are in `lore-io/BENCHMARKS.md`, alongside `examples/pool-sweep.sh` for the cap sweep and `examples/build-ab.sh` for an A/B of two builds.
- `lore-base/src/runtime.rs` — the core and net runtime accessors and the thread budget the engine is sized against.

## See also

- The enhancement proposal `docs/proposals/2026-07-24-tokio-runtime-split-and-async-io.md` records the decision, the thread-budget arithmetic, the rejected alternatives, and the migration slicing.
- [System design](../../explanation/system-design.md) — where the storage layer's fragment sizes come from.
