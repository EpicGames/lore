---
status: accepted
date: 2026-11-14
deciders: Zack Neyland
---

# ADR-00038: A commit intent orders a commit's anchor writes across flushes

## Context and Problem Statement

[ADR-00014](00014-anchor-tracking-in-mutable-store.md) moved the current and staged anchors into the
mutable store, keyed by the instance, and counted on one flush writing a commit's revision data,
branch latest and anchors atomically. A flush is not atomic across keys. The mutable store writes
each changed bucket to its own file, so a process killed while it flushes can leave a commit's
branch latest on disk without its current or staged anchor, or its anchors without its branch
latest. The next commit then reads the branch as advanced by another instance, though no other
instance committed, and an amend in that state amends the old revision and orphans the committed
one.

How does a commit keep its anchors consistent with its branch latest when the store can land its
keys one at a time?

## Decision Drivers

- A commit killed at any point leaves the next command enough to finish it or to leave it as it
  is, never anchors ahead of the branch latest.
- Recovery never takes work staged after the kill for the commit's own.
- A store that fails a read fails the command, rather than reading as an absent key.
- No change to the store's on-disk format, and a client built before this decision can still use
  the store.

## Considered Options

- Make the mutable store's flush atomic across buckets
- Order the writes across flushes, recording the revision being published as a commit intent
- Detect the torn state at the next commit and repair it from the branch latest alone

## Decision Outcome

Chosen option: "Order the writes across flushes, recording the revision being published as a
commit intent", because it needs no change to the store and leaves a record of what the killed
commit published, which the branch latest alone does not give.

A commit or amend stores the revision it publishes as the instance's commit intent, under the
`anchor-committing` key, and flushes the stores. It then stores the branch latest and flushes,
writes the current anchor (and, for a commit, the staged anchor) and flushes, and clears the
intent. The anchors never reach disk ahead of the branch latest, and the cleared intent never
reaches disk ahead of the anchors. A link's or nested link's own commit keeps its single write
and no intent, since the commit that follows reads a link's revision from its branch latest or
pin.

A commit or amend that finds a non-zero intent checks it before it opens its filesystem operation.
The intent names a commit cut short where it is the branch latest and the anchors are behind it: the
current anchor is on a revision the intent's commit was made over (its first parent; its second
parent where both parents record the same branch, as a sync of divergent history leaves them; or a
revision with the same parents, which an amend replaces), or the current anchor is on the intent and
the staged anchor is still staged over the commit's first parent.

- Where nothing was staged since, the command finishes the commit: it makes the intent's revision
  current, hands it to a filesystem provider that holds its state to hold, rebases the staged anchor
  onto it, stores the last sync where the commit merged divergent history, and clears the intent.
  A staged delete of a path the intent's revision no longer holds is the commit's own.
- Where something was staged since, which the staged anchor shows by staging other parents or
  anything the intent's revision does not hold, or by staging anything over the intent's revision,
  the command fails with `BranchAdvanced` and leaves the intent and the anchors as they are. A
  forced command instead clears the intent and runs over the anchors as they are, as it did before
  commits recorded an intent.
- An intent that names no commit cut short is cleared.

Recovery reads the intent, the current anchor, the branch latest and the staged anchor strictly:
only an absent key reads as zero or as no staged state, and any other read error fails the command
and keeps the intent. Every other write of the current anchor that finds a non-zero intent clears it
and flushes before it writes the anchor.

This supersedes ADR-00014, and carries forward the rest of its decision unchanged. The current and
staged anchors live in the mutable store as revision hashes, with no branch ID, under `Untyped` keys
derived from a per-clone instance ID (a UUIDv7 in `.lore/instance`). Each instance is registered
under `KeyType::Instance`, pointing to its metadata in the immutable store, and a repository with
the older `.lore/current` and `.lore/staged` files migrates them on its first load after upgrade.
Only ADR-00014's claim that one flush writes a commit's revision data, branch latest and anchors
atomically is replaced, by the ordering above.

### Consequences

- Good, because a commit killed while it flushes no longer leaves the next commit failing as though
  another instance had committed.
- Good, because the intent is validated against the anchors rather than trusted, so an intent an
  older client left behind a move of the current anchor is cleared rather than acted on.
- Bad, because a commit or amend runs three inline flushes of the stores, four for a commit on
  FUSE, each with fsync where data sync is on, before it returns.
- Bad, because a client built before this decision neither writes nor clears the intent. An older
  client that moves the current anchor back to exactly a revision the intent's commit was made over,
  while the branch latest stays on the intent, leaves a state the next newer commit takes for a
  commit cut short.
- Neutral, because the intent is one new persisted key per instance that reads as zero when absent
  and that no older client reads, so the store's format does not change.

## Pros and Cons of the Options

### Make the mutable store's flush atomic across buckets

- Good, because every writer, not only commit, gains atomic flushes.
- Bad, because it changes the store's on-disk format, which needs an LEP and a migration, and an
  older client could not read the store.

### Order the writes across flushes, recording a commit intent

- Good, because the store and its format are unchanged.
- Good, because the intent tells a commit cut short apart from a branch another instance advanced.
- Bad, because each commit pays for the extra flushes.

### Detect the torn state from the branch latest alone

- Good, because it adds no key and no flush.
- Bad, because a branch latest ahead of the anchors reads the same whether this instance's commit
  was cut short or another instance on a shared store committed, so recovery could make another
  instance's revision current.

## More Information

The same torn write between the branch latest or the current anchor and the staged anchor remains in
`lore sync`, `lore branch reset` and `lore revision restore`. The glossary defines
[commit intent](../../glossary.md#commit-intent).
