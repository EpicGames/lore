---
status: accepted
date: 2026-11-17
---

# ADR-00040: Reduce future size by managing data lifetime, not by allocating

## Context and Problem Statement

The synchronous C API polls each command's future on the caller's thread, and the hot paths run
once per file, node block or diff item. Futures and poll frames had grown to tens of kilobytes.
Boxing the large futures would bound their size at the cost of a heap allocation per item.

How should Lore keep future and poll frame sizes bounded on hot paths?

## Decision Drivers

- No heap allocation per item (per fragment, file, node, request or connection) on a hot path.
- Bounded future and poll frame sizes on the caller's thread.
- A saving that a later edit can't undo unnoticed.

## Considered Options

- Box the large futures
- Reduce what each future holds, by managing the lifetime of its data, and box only cold paths

## Decision Outcome

Chosen option: "Reduce what each future holds, by managing the lifetime of its data, and box only
cold paths", because it bounds future sizes without adding an allocation on a per-item path.

A future's or a poll frame's size is reduced by managing the lifetime of the data it holds, never
by adding a heap allocation per item (per fragment, file, node, request or connection). Only a
cold path is boxed: a cache miss, an error or recovery path, or a rarely taken branch. Tree walks
are worklists, not recursion. The rules are in the [async futures standard](../code-standards/async-futures.md).

### Consequences

- Good, because hot paths allocate nothing extra per item.
- Bad, because shrinking a future takes longer than boxing it. Each change is measured against its
  base in a release type-size build, and a change whose saving a later edit could undo unnoticed
  also gets a relative size test.
- Bad, because call data lives on the stack, so some futures stay larger than a boxed design would
  make them.
- Bad, because hot paths that hold a large argument across an await use hand-written futures (an
  `fn` returning an `async move` block) in place of `async fn`, at some cost in readability.
- Neutral, because a box or `#[async_trait]` on a per-item path, or a spawn per item with no cap, is
  a review finding.

## Pros and Cons of the Options

### Reduce what each future holds, and box only cold paths

- Good, because a hot path adds no allocation per item.
- Bad, because each change needs a measurement against its base, and sometimes a size test.

### Box the large futures

- Good, because it's quick to apply and bounds the size at once.
- Bad, because it adds a heap allocation on every call of a hot path.

## More Information

The rules and the measurement method are in
[`async-futures.md`](../code-standards/async-futures.md).
