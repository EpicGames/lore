---
status: accepted
date: 2026-11-24
---

# ADR-00044: One writer task writes each QUIC stream's responses on the server

## Context and Problem Statement

[ADR-00043](00043-one-writer-task-per-quic-stream.md) gave each stream of the QUIC client one
writer task. Lore Server still wrote its responses the old way. Each request's handler ran on the
core runtime, took its stream's `tokio::sync::Mutex` over the `SendStream`, wrote its response and
released the lock.

Under a burst of small requests to a peer, the responses queued for that lock while the stream sat
idle between handoffs. The response metrics showed it: a response's wait for the lock was many
times longer than its handler and its write together. Each write also took quinn's connection lock
on a core worker.

A request rejected before its handler ran was answered from the stream's read loop, which awaited
the same lock. While a response was stuck on flow control, the read loop stopped reading. When the
client finished its side of the stream, the read loop finished the server's side straight away,
before handlers still running had written their responses.

How should the server write the responses of the requests that share a QUIC stream?

## Decision Drivers

- A stream is written as soon as a response is ready, without waiting for a waiter to be
  scheduled.
- quinn's connection lock is taken on the net runtime, not on core.
- Responses are never interleaved, and a response started is always finished.
- A request holds its stream permit and its connection admission until its response is written,
  so egress congestion cannot let a connection admit more work than it can answer.
- The read loop never waits for a response to be written, except to bound what a client that stops
  reading can make the server hold.
- The server's side of a stream is finished only after the last response.
- No allocation per response beyond the copy of its header
  ([ADR-00040](00040-reduce-future-size-without-allocating.md)).

## Considered Options

- Keep the per-stream lock
- One writer task per stream, with a queue of its own
- One writer task per stream, sharing the client's frame queue

## Decision Outcome

Chosen option: "One writer task per stream, sharing the client's frame queue", because it removes
the lock handoff and moves quinn's connection lock to the net runtime, as ADR-00043 does for the
client, and keeps one implementation of the queue both ends rely on.

`FrameQueue` in `lore-transport/src/quic/frame_queue.rs` holds the frames a stream's writer has
taken, each with an entry of its owner's type, and lays out every frame queued at once for as few
`write_chunks` calls as flow control allows. Its owner chooses how much room it keeps once it
empties. The client's `StreamQueue` keeps its reply entries and dropped commands in its entries.
The server's entries hold each response's permits and what its metrics report.

Each stream the server accepts has a channel and one writer task on the net runtime, spawned when
the stream is accepted. The task owns the stream's `SendStream`. A handler queues its response on
the channel with its permit and admission guard, and does not wait for the write. The writer takes
every queued response at once, writes them, then records each response's metrics and drops its
permits. A handled response also carries its request's span. The writer reports the response in that
span and keeps it open until the response is written or fails. A rejection is queued the same way,
holding a permit from a small per-stream pool. The read loop waits only once that pool is used up.
When the read loop and every handler have dropped their senders, the writer writes what is left and
finishes the stream.

### Consequences

- Good, because a stream is written while responses are queued, by the task that owns it, so no
  response waits for a lock handoff.
- Good, because quinn's connection lock is taken on the net runtime.
- Good, because the read loop keeps reading while a response waits for flow control.
- Good, because the stream is finished after the last response, not before handlers still running
  have written.
- Good, because a request's span lasts until its response is written, so a trace's request span
  includes the wait for the writer and the write, and the slow-response log carries the request's
  fields.
- Bad, because a response makes one extra runtime crossing, from core to net, before it is
  written.
- Bad, because the channel is unbounded. The stream permit pool bounds the handled responses on it,
  and the rejection pool bounds the rejections.
- Bad, because each stream holds a channel and a queue. The channel allocates a block of 32
  queued responses, about 10 KB on a 64-bit target, when the stream is accepted, and keeps up to
  three more for reuse after a burst. The queue keeps room for 8 responses once it empties, where
  the client's keeps room for 64 frames.
- Bad, because responses leave as soon as they are queued, in more and smaller packets than under
  the lock, where they piled up and left together, so a client's stream reader more often finds
  data waiting. quinn's reads don't use tokio's cooperative budget, so a reader that keeps reading
  while data waits holds its net worker and starves the tasks waiting to run there, such as the
  client's stream writers. The client's reader therefore takes a unit of the budget for each chunk
  and each response it reads, and the server's read loop does the same for each chunk and request.
  With both readers taking the budget, the slowest requests at 10,000 and 16,384 in flight on one
  connection are no slower than without this change, and at 10,000 slightly faster.
- Bad, because a response waiting for the client's flow-control credit now waits inside the
  writer's write rather than for the lock, so a stream of large responses gains little: with
  thousands of large responses in flight on one connection, the slowest take slightly longer
  while the median falls.
- Neutral, because a success response makes one allocation fewer than with the lock, as its chunks
  go straight into the queue, and an error response or a rejection makes one more, the copy of its
  header.
- Neutral, because a failed write fails every response queued on the stream, and the writer logs
  the failure once for the stream rather than once a response.
- Neutral, because `response.queue_duration` replaces `response.lock_wait_duration`. It measures
  from a response being queued until the writer takes it. `response.write_duration` measures from
  then until the response is written in full, so it includes waiting behind the responses queued
  before it.

## Pros and Cons of the Options

### Keep the per-stream lock

- Good, because it needs no change, and a handler writes on its own task with no runtime crossing.
- Bad, because under a burst each stream waits for the next waiter to be scheduled before it is
  written again.
- Bad, because quinn's connection lock is taken on core, and a rejection stops the read loop behind
  a stalled write.

### One writer task per stream, with a queue of its own

- Good, because it leaves the client untouched.
- Bad, because the server would carry a second copy of the queue's buffer handling: the frames
  written in place, the buffer written in two parts where it wraps, and the room given back after a
  burst.

### One writer task per stream, sharing the client's frame queue

- Good, because of the consequences above, with one queue for both ends.
- Bad, because the client's queue is split into the shared part and its own entries, and the
  dropped-command handling only the client needs lives on the shared type.

## More Information

The rules each writer keeps are in the doc comments of `FrameQueue` in
`lore-transport/src/quic/frame_queue.rs`, of `write_responses` in
`lore-server/src/quic/stream_handler.rs`, and of `write_stream` and `StreamQueue` in
`lore-transport/src/quic/client.rs`.
