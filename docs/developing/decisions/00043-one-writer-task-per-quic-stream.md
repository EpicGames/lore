---
status: accepted
date: 2026-11-20
---

# ADR-00043: One writer task writes each QUIC stream's frames

## Context and Problem Statement

The QUIC client in `lore-transport/src/quic/client.rs` sends each command as one frame on one of
a connection's streams. A command took its stream's `tokio::sync::Mutex` over the stream's
`SendStream`, wrote its frame, and released the lock.

Under a burst of requests this left streams idle. The mutex hands the lock to the next waiter,
and that waiter must be scheduled on the core runtime before it can write. With the core runtime
flooded, a stream waited between writes while its waiters queued. Each write also took quinn's
connection lock inside `write_chunks`, on a core worker. A load test showed both: a caller with
five peers and bursts of lookups, where a request's time was almost all the wait to be polled
again on the core runtime, and core workers blocked on these locks.

How should commands that share a QUIC stream write their frames to it?

## Decision Drivers

- A stream is written as soon as a frame is ready, without waiting for a waiter to be scheduled.
- quinn's connection lock is taken on the net runtime, not on core.
- Frames are never interleaved, and a frame started is always finished.
- A command dropped before its frame is written leaves no reply entry; a frame written in full or
  in part keeps its entry.
- No growth in the request futures, and no allocation per request
  ([ADR-00040](00040-reduce-future-size-without-allocating.md)).

## Considered Options

- Keep the per-stream lock
- Open more streams per connection
- One writer task per stream, fed by a channel
- One writer task per stream, with an inline fast path

## Decision Outcome

Chosen option: "One writer task per stream, fed by a channel", because it removes the lock
handoff and moves quinn's connection lock to the net runtime, and it keeps one write path.

Each stream has a channel and one writer task on the net runtime, spawned when the stream opens.
The task owns the stream's `SendStream`. A command queues its frame on the channel and awaits one
oneshot. The writer takes every queued frame at once, registers each frame's command as the reply
entry before any byte of it is written, and writes the frames in as few `write_chunks` calls as
flow control allows, and one more where the queue's buffer wraps. A failed write fails every
queued frame with the write's error. A command dropped before its frame is taken leaves nothing
registered; a frame not yet started is removed from the queue with its entry; a frame partly
written is copied and finished.

### Consequences

- Good, because a stream is written while frames are queued, by the task that owns it, so no
  command waits for a lock handoff.
- Good, because quinn's connection lock is taken on the net runtime.
- Good, because the request futures are smaller than with the lock, since a command holds no lock
  guard across its write.
- Bad, because a request makes one extra runtime crossing, from core to net, before its frame is
  written. A caller with few requests at once pays this on every request, for tens of
  microseconds each.
- Bad, because the channel is unbounded. The command permit pool bounds the frames on it. The
  callers that send outside the pool send once per connection or per session.
- Bad, because the writer holds the stream's queue lock, a `parking_lot` mutex, across
  `write_chunks`, within one poll and never across an await. A command dropped while its frame is
  queued waits for that write to take its frame back. The lock order is the queue lock, then
  quinn's connection lock.
- Neutral, because a command that receives a server's answer treats its frame as written in full
  (`shows_frame_written`), and does not take it back. This trusts the server to answer only after
  it has read the whole frame, as Lore's server does.

## Pros and Cons of the Options

### Keep the per-stream lock

- Good, because it needs no change, and a command writes on its own task with no runtime
  crossing.
- Bad, because under a burst each stream waits for the next waiter to be scheduled before it is
  written again.
- Bad, because quinn's connection lock is taken on core.

### Open more streams per connection

- Good, because it spreads the waiters over more locks.
- Bad, because it keeps the handoff on each stream. Measured in the load test, it gave no
  consistent gain.

### One writer task per stream, fed by a channel

- Good, because of the consequences above.
- Bad, because of the extra runtime crossing for every request, and the unbounded channel.

### One writer task per stream, with an inline fast path

The command writes its frame on its own task when the stream's queue is empty, and queues it for
the writer otherwise.

- Good, because a caller with few requests at once makes no runtime crossing.
- Bad, because a stream has two write paths, which must agree on frame order, reply entries and
  dropped commands.
- Bad, because the inline write takes quinn's connection lock on core again. The cost of the
  crossing it saves was accepted instead.

## More Information

The design rules the writer keeps, such as when a reply entry is registered and how a dropped
command's frame is handled, are in the doc comments of `write_stream` and `StreamQueue` in
`lore-transport/src/quic/client.rs`.
