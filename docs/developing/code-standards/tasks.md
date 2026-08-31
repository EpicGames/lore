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
| `lore_spawn_blocking!(task)` | Spawn blocking task with context |
| `lore_spawn_blocking_nocontext!(task)` | Spawn blocking task without context |
| `lore_spawn_guarded!(task)` | Spawn task that must complete before runtime shutdown |
| `lore_drain_tasks!(tasks, err)` | Drain JoinSet, propagate first error |
| `lore_limit_drain_tasks!(tasks, max, err)` | Non-blocking drain with bounded concurrency |

All `lore_spawn!` variants automatically propagate `LORE_CONTEXT` if one is set in the calling task. If no context is
set, the task is spawned without context scoping.

The blocking variants accept all the same forms (bare, named, JoinSet, named JoinSet).

## Parallel tasks

Prefer `JoinSet` over storing individual `JoinHandle`s for multi-task coordination. Use `lore_drain_tasks!` for simple
parallel operations with first-error semantics. When you need to process individual results, drain the JoinSet manually
with `join_next().await`.

## Task cancellation

Use `AbortOnDropHandle` (from `tokio_util`) for background tasks that shouldn't outlive their scope. Wrap the
`lore_spawn!` return value in `AbortOnDropHandle::new(...)` and the task is automatically aborted when the handle is
dropped.

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
4. **Use `lore_spawn_blocking!`** for CPU-bound work to avoid blocking the async runtime.
5. **Use `.in_current_span()`** on all spawned tasks where the `tracing` crate is a dependency.
6. **Handle JoinError** — Map task panics and cancellations to your error type via `.internal("...")`.
