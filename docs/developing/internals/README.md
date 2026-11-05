# Internals

Implementation-detail reference: how Lore is built under the hood.

## What this folder is

Precise technical descriptions of Lore's source: byte layouts, struct formats, serialization specs, internal protocols. Austere, authoritative, descriptive only. No instruction or explanation; just how the machinery is built.

## What you'll find here

- [File I/O engine](file-io-engine.md) — the `lore-io` driver, buffer ownership, syscall pool, and the plan for replacing `std::fs` and `tokio::fs`.
- [Parallel path staging](parallel-path-staging.md) — how `lore stage` normalizes a target set, pre-creates the directories targets share, and fans the walks out.
- [State node architecture](state-node-architecture.md) — how a `State` holds its revision tree in node blocks, and how the node operations change it under concurrent writers: the tiers, the block locks, the checks under them, saving, and sweeping detached slots.

## Suggested starting points

- **Writing a new Internals page?** Start at the [doc-standards walkthrough](../doc-standards/writing-a-doc.md).

See [docs/README.md](../../README.md) for the full docs structure.
