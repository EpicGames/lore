---
status: superseded by [ADR-00038](00038-commit-intent-orders-anchor-writes.md)
date: 2026-03-21
deciders: Mattias Jansson
consulted: Raghav Narula, Matt Hoffman
---
# ADR-00014: Use mutable store for anchor tracking

## Context and Problem Statement

Lore currently stores the current and staged anchors (which revision and branch a clone has checked out) as separate 48-byte files (`.lore/current` and `.lore/staged`) in each repository's `.lore/` directory. This works for single clones but doesn't support multi-clone workflows where multiple clones of the same repository share a global store. With file-based anchors, clones can't see each other's checkout state, preventing branch checkout awareness and requiring worktree-like abstractions to coordinate between clones.

Additionally, the anchor file write is a separate operation from the store flush, creating a crash recovery window where the store has been updated but the anchor file hasn't (or vice versa).

## Decision Drivers

* Enabling shared mutable state across multiple clones using the global store without a dedicated worktree abstraction
* Consolidating storage in as few separate systems and serialized paths as possible, consistent with ADR 00002 (branch tracking in mutable store)
* Improving crash recovery by making anchor updates atomic with other mutable store writes
* Simplifying the anchor model by removing the redundant branch ID (derivable from the revision)

## Considered Options

### Keep the current separate files

The anchor is written as a 48-byte file (revision hash + branch ID) in `.lore/current` and `.lore/staged`. Each clone's anchor is invisible to other clones. Branch checkout awareness across clones would require a separate registry or filesystem scanning. Crash recovery has a window between store flush and anchor file write. The branch ID in the anchor is redundant since every revision in Lore belongs to exactly one branch.

**Good**:

* Simple and human-readable — can inspect checkout state by reading a file
* No dependency on the mutable store for basic repository state
* No migration needed for existing repositories

**Bad**:

* Clones can't see each other's checkout state — multi-clone awareness requires a separate registry or worktree abstraction
* Crash recovery window between store flush and file write — anchor can be inconsistent with store state
* Redundant branch ID in anchor adds unnecessary complexity to the Anchor struct
* Separate serialization path from branch pointers — two different mechanisms for similar mutable state
* Anchor files need separate handling in backup and data relocation

### Use the mutable store

The anchor is stored as a revision hash in the mutable store, keyed by a per-clone instance ID (UUIDv7 stored in `.lore/instance`). The branch ID is removed from the anchor since it can be derived from the revision. For local-store repositories, anchors are in the local `.lore/mutable/` store. For global-store repositories, anchors are in the shared global mutable store, making them visible to all clones sharing that store. Branch checkout awareness follows directly from enumerating instances and reading their anchors. Anchor writes are part of the same mutable store flush as branch pointer updates, making them atomic. No additional files or serialization methods are needed.

**Good**:

* Multi-clone checkout awareness without a dedicated worktree abstraction — anchors are visible across clones sharing a global mutable store
* Atomic crash recovery — anchor, branch pointer, and revision data are flushed together in a single operation
* Simplified anchor model — branch ID removed, just a revision hash fitting the mutable store's Hash interface
* Unified storage — all mutable state (branch pointers, sync state, anchors) in one system, consistent with ADR 00002
* No additional backup or relocation considerations — anchors are part of the existing store

**Bad**:

* Anchors are no longer human-readable — requires tooling to inspect checkout state
* Requires instance IDs (`.lore/instance`) as a new per-clone concept
* Lazy migration needed for existing repositories (one-time, transparent)
* Instance ID file corruption loses checkout state (same blast radius as anchor file corruption today)

## Decision Outcome

Use the mutable store for anchor tracking. This unifies anchor storage with branch pointer storage, enables multi-clone awareness through the shared global mutable store, simplifies the anchor model by removing the redundant branch ID, and improves crash recovery by making all mutable state updates atomic in a single flush.

Each clone gets a stable instance ID (UUIDv7 in `.lore/instance`) used to derive anchor keys. The instance is registered in the mutable store with a new `KeyType::Instance` pointing to metadata (filesystem path, creation time) in the immutable store. Anchors use `Untyped` keys derived from the instance ID.

### Consequences

* Anchors are no longer human-readable files — they're opaque entries in the mutable store, consistent with branch pointers after ADR 00002.
* All repositories (with or without global store) gain instance IDs and store-based anchors. This is a universal change, not limited to global store users.
* The `.lore/current` and `.lore/staged` files are removed. Existing files are migrated on demand on first repository load after upgrade.
* Crash recovery improves — a single flush writes revision data, branch pointer, and anchor atomically. Pre-flush crashes lose nothing; post-flush crashes persist everything.
* Multi-clone branch checkout awareness becomes possible without a dedicated worktree abstraction. When clones share a global mutable store, each clone's anchor is visible to all others.
