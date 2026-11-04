---
status: proposed
date: 2026-11-03
deciders: Zack Neyland
---

# ADR-00027: Keep modified-time records in the instance's dot directory

## Context and Problem Statement

[ADR-00014](00014-anchor-tracking-in-mutable-store.md) put anchors in the mutable store so that instances
sharing a store can see each other's checkout state. Modified-time records live in the same store: one
record per file, keyed by a hash of the instance and the path (`file_modified_time_key` in
`lore-revision/src/state.rs`), in the buckets every instance loads and rewrites under the store lock.

A record is 84 bytes in the store (`MutableStoreEntry` in `lore-storage/src/local/mutable_store.rs`,
plus its index entry), and Lore keeps one per file: a working tree of 1.1 million files holds about 92 MB of
records per instance. No other instance reads them, and a copy of the instance given an identity of its own
cannot use them, because they name the instance ID.

## Decision Drivers

- State that only one instance reads should not grow the store every instance loads.
- A copy of an instance's directory should carry valid records with it.
- No client, before or after an upgrade, should trust a record for content its path no longer holds.
- Anchors and the instance registry stay shared: other instances read them.

## Considered Options

- Keep modified-time records in the shared mutable store, and copy them for a new instance
- Keep modified-time records in a per-instance mutable store in the dot directory
- Keep modified-time records in a record file of their own in the dot directory

## Decision Outcome

Chosen option: "Keep modified-time records in a record file of their own in the dot directory", because it
keeps the store from growing with every file of every instance, it travels with a copy of the directory at
no extra cost, and its entry is 24 bytes against the store's 84.

Records live in `.lore/records/`, keyed by the path. A lookup that misses there falls back to the store
record an older client wrote, and an upgraded client zeroes a path's store record when it records a new time
for the path or clears it, so the fallback does not answer with a stale time. SWFS instances, whose mount
presents the recorded times, and in-memory instances keep their records in the store. Anchors and
registrations stay in the mutable store as ADR-00014 decided. The
[instance-local modified-time records](../../proposals/2026-11-03-instance-local-modified-time-records.md)
LEP gives the design.

### Consequences

- Good, because a copy of an instance starts with records that describe its files.
- Good, because a new instance adds no records to the shared store. Records older clients wrote stay
  there, zeroed once their paths change, at 84 bytes each, as zeroed keys do today.
- Bad, because Lore does not write the records atomically with the store flush, which ADR-00014 relies on
  for anchors. A lost record costs a hash, never a wrong answer.
- Bad, because a client from before this change reads only the store; after a downgrade it hashes the
  files whose records moved.
- Neutral, because restoring committed modification times on sync stays possible.
