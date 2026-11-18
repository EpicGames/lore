---
status: accepted
date: 2026-11-17
---

# ADR-00041: A thread that blocks for file operations runs them

## Context and Problem Statement

The file I/O engine ran every `psync` operation, and the control-plane operations of the
completion backends, as a job on the syscall pool. That was deliberate: a job run inline on an
async worker would block the worker
([file I/O engine](../internals/file-io-engine.md#syscall-pool)).

SWFS called each of a mount's callbacks on a thread of its own and waited for the answer, and the
callback polled its future on that thread in `block_on` until it finished. Each file operation the
callback started was handed to a pool thread while the calling thread slept, which cost two thread
hand-offs and freed nothing.

Where may a file operation run on the thread that starts it?

## Decision Drivers

- No thread hand-off that frees no thread, on a path that runs per request.
- An async worker never blocks on a file operation.
- The engine's operations keep one implementation.

## Considered Options

- Keep every job on the pool
- Select the `iocp` backend
- Let a thread that blocks for its operations run them itself

## Decision Outcome

Chosen option: "Let a thread that blocks for its operations run them itself", because it removes the
hand-offs where they free nothing and leaves every other caller as it is.

`lore_io::run_file_operations_on_this_thread(call)` marks the calling thread for the length of
`call`, and `SyscallPool::submit` runs a marked thread's job before it returns. A caller uses it
only when all of these hold:

- The thread isn't an async runtime worker.
- The thread blocks until the operations it starts within `call` finish.
- The work awaits its file operations one at a time.

A caller that doesn't meet all three submits to the pool.

### Consequences

- Good, because a callback of an SWFS mount doesn't wait on two thread hand-offs for each file
  operation its own task starts.
- Good, because callers that don't opt in are unchanged.
- Neutral, because tasks the marked work spawns run on runtime workers and still use the pool. The
  leaf reads of content stored as a fragment list are such tasks.
- Bad, because the mark is ambient: it applies to all code `call` reaches on the thread, so a
  caller that wraps work on an async worker blocks that worker. The function's documentation
  states the rule.
- Bad, because a marked thread runs its file operations one after another: joined operations don't
  run at once, and a timeout around one can't fire while it runs.
- Bad, because a job a thread runs itself is outside the pool's cap and its counters.

## Pros and Cons of the Options

### Keep every job on the pool

- Good, because every job runs in one place, within the cap.
- Bad, because the thread that calls a mount's callback pays two hand-offs for each file operation
  while it only waits.

### Select the `iocp` backend

- Good, because it needs no code change.
- Bad, because it gains little here. Measured on an SWFS instance of a large game repository, the
  first 1,000 file reads after a service start took 1.59 s against 1.62 to 1.65 s on `psync`, where
  running the jobs on the calling thread took 1.29 to 1.41 s. The opens and whole-file reads stay
  on the pool on every backend.
- Bad, because `auto` selects `psync` on every platform, for the reasons the
  [file I/O engine](../internals/file-io-engine.md#driver-and-backend-selection) page gives.

### Let a thread that blocks for its operations run them itself

- Good, because the hand-offs go where they free nothing.
- Good, because the job runs through the same task as on the pool, so results and panics reach the
  awaiter the same way.
- Bad, because of the ambient mark, the one-at-a-time order and the cap, as above.

## More Information

The proposal
[A Bounded Thread Model](../../proposals/2026-07-24-tokio-runtime-split-and-async-io.md) states
why `psync` runs its operations on a pool rather than inline. This decision keeps that for every
thread the reason applies to, and adds the case it doesn't cover: a thread that isn't a runtime
worker and only waits.

The ProjFS callbacks in `lore-revision/src/projfs/serve.rs` have the same shape and don't use the
mark: nothing was measured there.
