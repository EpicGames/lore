---
status: proposed
date: 2026-11-09
deciders: Zack Neyland
---

# ADR-00030: Exclusive holders of a repository lock within a process

## Context and Problem Statement

Commands in one process that opened the same instance shared one holder of its repository lock:
the flock was taken once, and later commands joined the holder while it lived
(`REPOSITORY_LOCK_CACHE` in `lore-revision/src/repository.rs`,
[ADR-00024](00024-store-locks-held-per-span-contain-the-repository-lock.md)). The flock kept out
other processes only. An instance clone had to keep the Lore commands that hold the source's lock off
the source while it copied the source's tree, including the commands the Lore service runs for other
clients, and read commands took no write token that would have made them wait.

## Decision Drivers

- No command may change or read a half-copied source through a holder it shares with the clone.
- The shared path that every other command takes must not get slower.
- The order of locks must stay the one ADR-00024 fixes.

## Considered Options

- An exclusive holder: a per-directory gate in the process that shared holders read and an
  exclusive holder writes.
- A write token held by the clone, which read commands do not take.
- Copying outside the locks.

## Decision Outcome

Chosen option: "An exclusive holder", because it is the only one that keeps read commands out
too. `LockSharing::Exclusive` makes `get_or_create_repository_lock` wait until no holder for the
directory is alive in the process, create one of its own, and keep later callers from joining
it; they wait for it instead. The gate is taken after the store locks and before the flock, so
the order is write token, store locks, gate, flock.

### Consequences

- Good, because a command on the source in the same process, a read included, waits for the
  clone.
- Good, because a shared caller that finds a live shared holder joins it without waiting at the
  gate unless an exclusive caller waits, so the common path is as before.
- Bad, because an exclusive caller waits for the commands that hold the lock when it comes,
  however long they run. While it waits, a shared holder admits no more commands: they queue
  behind the exclusive caller, as a waiting writer holds back later readers in
  [ADR-00026](00026-concurrent-read-only-instance-operations.md), so overlapping commands, such
  as a client polling status, cannot keep it out for good.
- Bad, because commands on other instances over the same store share the process's hold of the
  store locks and still run during the clone.
- Bad, because commands from other processes on every instance over the store wait for the whole
  clone, which holds the store locks for as long as its copy takes, a clone that shares blocks
  included.

## Pros and Cons of the Options

### An exclusive holder

- Good, because read commands in the process wait for the clone as write commands do.
- Good, because the order of locks stays the one ADR-00024 fixes, with the gate between the store
  locks and the flock.
- Bad, because commands on other instances over the store still share the process's hold of the
  store locks and run during the clone.

### A write token held by the clone

- Good, because write commands already take the token, so it adds no lock.
- Bad, because read commands take no token, so a status in the service could read a source the
  clone is copying, through the holder it shares with the clone.

### Copying outside the locks

- Good, because nothing waits for the copy.
- Bad, because a sync or commit could change the source and its anchors mid-copy, so the clone
  could pair one revision's anchors with another revision's files.

## More Information

The [copy-on-write instance clone](../../proposals/2026-10-31-copy-on-write-instance-clone.md) LEP
describes the clone this holder serves.
