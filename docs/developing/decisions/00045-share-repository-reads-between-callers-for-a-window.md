---
status: accepted
date: 2026-11-25
---

# ADR-00045: Share a repository's mutable reads between a server's callers for a window

## Context and Problem Statement

All mutable keys of one repository share one partition of the mutable store. Bursts of
`BranchList` calls on one large repository each read every branch entry of it, and the store
throttles that partition. Range reads make one call cheaper, but the read units still grow with the
call rate.

How should a server bound the mutable reads that many callers of one repository-wide read cause?

## Decision Drivers

- The reads one repository costs a server must not grow with the call rate.
- A listing may be about as out of date as an eventually consistent read already allows.
- A read that fails must not be repeated by every caller its error reaches.
- One caller going away must not stop or waste a read that other callers wait for.

## Considered Options

- Share only reads that are still running
- Share each read for a window after it starts, its error included
- Cache results for longer, or keep a stored index of branches

## Decision Outcome

Chosen option: "Share each read for a window after it starts, its error included", because it is
the only option that bounds the reads per repository by time rather than by call rate, without a
new write on other paths.

A read shared this way uses `lore_server::util::shared_read::SharedReads`:

- Reads are shared per key in one process. The key holds only what changes the reads. Each caller
  applies its own filters to the shared result.
- A caller always joins a running read of its key, so a key has at most one read in flight. It uses
  a finished read, or its error, if that read started less than the window ago. Otherwise it starts
  a new read. A zero window shares only running reads.
- A read runs in a task of its own, without the cancellation of the request that started it, in an
  execution context of its own with a new correlation id. Each caller's span names that id.
- A read removes its own entry once the window has passed since it started. Callers that hold the
  result keep it until they finish.
- Metrics count the reads started, the callers by how they were served, and each read's time.

### Consequences

- Good, because a repository costs at most one read per window per server process, whatever the
  call rate, on success and on a list that fails as it starts, whose error is shared.
- Bad, because a read whose list the store cuts short is shared too. The AWS mutable store ends a
  list early, without an error, at a page still throttled after its retries, so every caller in
  the window gets the same short listing.
- Good, because the building block is generic over the key, the result and the error, so another
  repository-wide read can share the same way.
- Bad, because a caller can get a result up to the window older than the read alone allows.
- Bad, because a read holds its whole result in memory for the window, with no cap.
- Bad, because sharing is per process: a deployment of N processes can read a repository N times
  per window.

## Pros and Cons of the Options

### Share only reads that are still running

- Good, because results are never older than the read.
- Bad, because reads take tens of milliseconds, so few callers in a burst join one, and the reads
  per second still follow the call rate.

### Cache results for longer, or keep a stored index of branches

- Good, because callers read the store even less.
- Bad, because a longer cache makes listings more out of date than eventually consistent reads.
- Bad, because a stored index adds a shared write to every push, create and delete.

## More Information

The first user is the `lore.revision.v1` `BranchList` handler, with the setting
`[feature] branch_list_share_window_millis` (1,000 ms by default, at most 60,000 ms).

One `BranchList` read lists the name, metadata and latest entries of a repository with one
eventually consistent range read each. On a DynamoDB mutable store that costs about one read unit
per 80 entries: about 310 read units for a repository of about 7,900 live and 700 archived
branches, against a partition limit of about 3,000 read units per second. In bursts of 30 to 70
calls per second on such a repository, reads per call exceed that limit by far. Shared for one
second, the reads cost each server process at most one read of the repository per second, whatever
the call rate.
