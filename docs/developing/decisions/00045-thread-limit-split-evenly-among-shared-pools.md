---
status: accepted
date: 2026-11-25
---

# ADR-00045: A thread limit is split evenly among the shared pools

## Context and Problem Statement

The [runtime split and async I/O proposal](../../proposals/2026-07-24-tokio-runtime-split-and-async-io.md)
gave Lore one thread limit, `LORE_MAX_THREADS` or `lore_set_thread_limit`, to be divided between
the core workers, the net runtime, the syscall pool and the blocking pool "by a straightforward
calculation". Lore divided it in proportion to each pool's request. The core workers ask for one
thread per processor and the syscall pool for at most 16, so on large hosts a limit left the
syscall pool a small part of its request. At a limit of 44 on 192 processors a clone ran its file
I/O on 3 threads, and every file operation of a clone goes through that pool. Keeping the
syscall pool's whole request instead starved the core workers under small limits: a limit of 16
left a command 2 core workers and 10 syscall threads.

How should a thread limit be divided between the pools?

## Decision Drivers

- Neither the core workers nor the syscall pool, the two pools a clone keeps busy, starves the
  other under a limit.
- A small fixed pool, which other work blocks on, keeps its size where the limit allows.
- The limit stays a ceiling on the total, and every pool keeps at least `MIN_THREADS_PER_POOL`.
- Without a limit, every count stays as requested.
- One rule for every process: commands, the service, embedders and the server.

## Considered Options

- Scale every pool in proportion to its request
- Keep the syscall pool's request and take the limit from the pools that ask for one thread per
  processor
- Keep the blocking and fixed net pools, and split the rest evenly among the shared pools

## Decision Outcome

Chosen option: "Keep the blocking and fixed net pools, and split the rest evenly among the shared
pools", because it gives the syscall pool its request where the limit allows, without taking the
core workers down to the minimum under small limits.

The fixed pools are the blocking pool and a net pool sized by default, by storage connections, by
`[tokio] net_threads` or by `LORE_NET_THREADS`. The shared pools are the core workers, the syscall
pool, and the net pool where `serving_runtime` asks for one thread per processor. Under a limit
the fixed pools keep their requests, and the shared pools split the rest evenly, each capped at
its request; what a capped pool leaves goes evenly to the others. Odd threads go to the core
workers first, then a per-processor net pool, then the syscall pool. Where the rest is too small
for the minimum of each shared pool, those keep the minimum and the fixed pools are scaled into
what is left. `apportion_thread_counts` in `lore-base/src/runtime.rs` implements the rule. A new
pool joins the budget as fixed or shared.

### Consequences

- Good, because a limit leaves the syscall pool at its request once its even share reaches it: 16
  threads at a limit of 44 on 192 processors, where it had 3.
- Good, because under a small limit the core workers and the syscall pool split it: at a limit of
  16 a command with one storage connection runs 6 core workers and 5 syscall threads.
- Good, because a command's net threads, one per storage connection, keep their count under a
  limit.
- Bad, because a pool's share no longer follows its request: a syscall pool raised by
  `LORE_IO_POOL_THREADS` gets no more than the core workers' share under a limit.

## More Information

The CR that implements this carries tables of the counts before and after, per process, processor
count and limit, and the unit test `the_counts_each_process_runs_under_a_limit` pins them.
