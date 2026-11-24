# Lore task spawning standards

This document defines the rules for spawning and managing async tasks across the Lore codebase.

## Core macros

Defined in `lore-base/src/runtime.rs`. All task spawning MUST use these macros (or the crate-specific alternatives
described below) to ensure `LORE_CONTEXT` propagation.

| Macro | Purpose |
| --- | --- |
| `lore_spawn!(task)` | Spawn async task |
| `lore_spawn!("name", task)` | Spawn named async task |
| `lore_spawn!(joinset, task)` | Spawn into JoinSet |
| `lore_spawn!(joinset, "name", task)` | Spawn named task into JoinSet |
| `lore_spawn_blocking!(task)` | Spawn a short blocking OS call onto the core blocking pool, with context. Not for CPU work: see [Where work runs](#where-work-runs) |
| `lore_spawn_guarded!(task)` | Spawn task that must complete before runtime shutdown |
| `lore_drain_tasks!(tasks, err)` | Drain JoinSet, propagate first error |
| `lore_limit_drain_tasks!(tasks, max, err)` | Non-blocking drain with bounded concurrency |

All `lore_spawn!` variants automatically propagate `LORE_CONTEXT` if one is set in the calling task. If no context is
set, the task is spawned without context scoping.

The blocking variants accept all the same forms (bare, named, JoinSet, named JoinSet).

## Where work runs

A Lore process runs a fixed set of thread pools, sized when the process starts (`lore-base/src/runtime.rs`;
[the runtime split proposal](../../proposals/2026-07-24-tokio-runtime-split-and-async-io.md) gives the design). Each
kind of work has one home:

| Work | Where it runs | How |
| --- | --- | --- |
| Async logic and CPU work: hashing, compression, chunking, parsing, diffing, merging | Core runtime workers, about one per processor | Inline in a core task, or `lore_spawn!` from one. From net, `lore_spawn_core!`: `lore_spawn!` follows the runtime it is called on |
| Network transport: QUIC endpoints, tonic channels and anything that dials out | Net runtime | Built under `lore_spawn_net!`. Request futures are awaited from core |
| File I/O | `lore-io` engine | `lore-io` calls, not `tokio::fs` or `std::fs` |
| Blocking OS calls with no async form: keyring, AWS SDK initialization, IPC pipe reads, a `flock` | Core blocking pool, a few threads for the whole process (`CORE_BLOCKING_THREADS`) | `lore_spawn_blocking!` |

### CPU work runs inline, and is bounded first

Run CPU work inline on the core workers. Don't move it to the blocking pool to "avoid blocking the runtime": the
workers are where CPU work is meant to run, and there are about as many of them as processors. The blocking pool has
only a few threads. One long job there holds a thread of it, and once a job holds every thread, all other blocking work
in the process waits behind them.

Inline work has to stay small, because a worker running it serves nothing else until it returns. The fragment-sized
work the runtime is designed around takes at most a few milliseconds. Where the cost of a piece of work grows with its
input (file size, line count, entry count, anything a client or a repository supplies), bound it before it starts:

- **Cap the input that drives the cost.** Check it before the work starts, and give the oversized case a correct
  fallback, such as leaving a conflict for the user to resolve or reporting the item as too large. Name the cap as a
  constant, and say at the constant what happens when an input exceeds it.
- **Bound the time as well as the memory.** A byte cap bounds memory, but not the time of an algorithm whose cost grows
  faster than its input. Myers diff, for example, takes time proportional to the input length times the number of
  differences. Two very different 16 MiB files can keep it busy for hours. Cap whatever drives the cost (for a diff,
  the line count of each side), or use an algorithm with a cost or time limit.
- **Remember that cancellation can't stop it.** A synchronous call has no `.await` at which a dropped request can stop
  it, so a timed-out request's work runs to the end, whether inline or on the blocking pool.
- **Cap how many run at once** when one item still costs more than fragment-scale work. Use a semaphore or a bounded
  stream, so that concurrent requests can't take every worker.

### The blocking pool is shared with libraries

Libraries running on the core runtime put their own work on the same pool. `tokio::fs`, `tokio::net::lookup_host` and
hyper's default DNS resolver all call `spawn_blocking`, and clippy can't see those calls. So:

- A long-running `lore_spawn_blocking!` closure starves code that never names the pool. For example, a gRPC channel
  built on core needs a blocking thread for its DNS lookup each time it reconnects. If every thread is held, the channel
  can't reconnect.
- Build network clients on the net runtime, so their connection work stays off the core pool. This includes telemetry
  exporters and any other client set up at startup.

## Parallel tasks

Prefer `JoinSet` over storing individual `JoinHandle`s for multi-task coordination. Use `lore_drain_tasks!` for simple
parallel operations with first-error semantics. When you need to process individual results, drain the JoinSet manually
with `join_next().await`.

## Task cancellation

Use `AbortOnDropHandle` (from `tokio_util`) for background tasks that shouldn't outlive their scope. Wrap the
`lore_spawn!` return value in `AbortOnDropHandle::new(...)` and the task is automatically aborted when the handle is
dropped.

When its owner is dropped, detach rather than abort a task that must not stop part way, such as a request that isn't
cancel safe. A dropped `JoinHandle` detaches its task, so one such task needs no wrapper. Where an owner can await more
than one such task, spawn them into a `DetachingJoinSet` (from `lore_base::runtime`), which detaches its tasks when
dropped.

Stop request work through the request's cancellation rather than by aborting its tasks. A spawned task inherits the
cancellation with `LORE_CONTEXT`. See [Cancellation](async-futures.md#9-cancellation).

## Error handling

Map `JoinError` (task panics/cancellations) to your error type via `.internal("...")` — it is a foreign error. Use `lore_drain_tasks!` as shorthand
for collecting the first error from a JoinSet while allowing all tasks to complete.

## Crate-specific patterns

### Library crates

**Crates:** `lore-base`, `lore`, `lore-revision`, `lore-notification`

Use `lore_spawn!` macros directly. Context propagation is automatic.

### Server crate (`lore-server`)

`lore_spawn!` works inside any task where `LORE_CONTEXT` is already set. This covers most handler code since gRPC and
QUIC entry points establish context before dispatching.

Manual `LORE_CONTEXT.scope()` with `runtime().spawn()` is only required at **entry points** where a new execution
context must be established (for example, gRPC handler top-level, QUIC connection accept, or background service init).
Once inside a scoped task, child tasks can use either `lore_spawn!` (automatic propagation) or
`runtime().spawn(LORE_CONTEXT.scope(execution_context(), ...))` (explicit propagation).

### External service crates (`lore-aws`, `lore-hashicorp`)

These crates don't participate in `LORE_CONTEXT` propagation. Use raw `tokio::spawn` or `joinset.spawn()`.

However, where `tracing` is a workspace dependency (as in `lore-aws`), spawned tasks MUST use `.in_current_span()` to
preserve tracing span parentage. Without it, spawned tasks run in a detached span and lose the trace link back to the
originating request.

## Best practices

1. **Always use `lore_spawn!` in library code** for automatic context propagation.
2. **Use `lore_spawn!` in server code** when context is already set; use manual `LORE_CONTEXT.scope()` only at entry points.
3. **Prefer JoinSet** over individual `JoinHandle`s for multi-task coordination.
4. **Run CPU work inline, bounded by a cap on its input**, and keep `lore_spawn_blocking!` for short blocking OS calls
   (see [Where work runs](#where-work-runs)).
5. **Use `.in_current_span()`** on all spawned tasks where the `tracing` crate is a dependency.
6. **Handle JoinError** — Map task panics and cancellations to your error type via `.internal("...")`.
