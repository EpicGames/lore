---
status: accepted
date: 2026-11-17
---

# ADR-00039: Reads record the metadata of the fragments they fetch

## Context and Problem Statement

A read with `ReadOptions::cache` off stored nothing locally for a fetched fragment without
`PayloadLocalCachePriority`, though the fetch had just learned that the remote holds the fragment
durably. A later put or push of the same content then found nothing locally and uploaded it again.
A client that pulls a large image on every node and then publishes the next version paid that
upload on every node.

Should a read that fetches a fragment, and doesn't keep its payload, record anything locally?

## Decision Drivers

- A put or push after a read uploads nothing the read showed the remote already holds.
- Whether a payload is kept stays the caller's choice.
- One rule for every reader of the shared read path.
- A reader that must leave the local store untouched can still do so.

## Considered Options

- Store nothing for a fetch whose payload isn't kept
- Record the fetched fragment as a header-only entry marked stored durable, for every reader
- Record it only for the storage API

## Decision Outcome

Chosen option: "Record the fetched fragment as a header-only entry marked stored durable, for every
reader", because it's the only option that removes the repeated upload for every reader without
changing which payloads are kept.

Every read that fetches a fragment and doesn't keep its payload records the fragment in the local
store as a header-only entry marked stored durable (`store_fetched` in `lore-storage/src/read.rs`).
This holds for every reader of the shared read path: the storage API, revision reads, the virtual
file system and prefetch, and the resolve of a fragmented root. `ReadOptions::no_local_write` is
the only opt-out. `ReadOptions::cache`, and `local_cache` in the storage API, govern whether the
payload is kept, never whether the metadata is.

### Consequences

- Good, because a put or push after a read skips the upload of every fragment the read fetched,
  with no server round trip.
- Bad, because the local index gains one entry per fetched fragment, and a flush writes more. Index
  memory grows with what a store has read, not only with what it keeps.
- Bad, because a local existence answer can name content the store holds no payload for, as the
  put path's header-only entries already do. Code that reads the local store mustn't take an
  entry to mean a payload.
- Neutral, because a reader that must leave the local store untouched has to set `no_local_write`.

## Pros and Cons of the Options

### Record the fetched fragment as a header-only entry, for every reader

- Good, because no reader of the shared path uploads content its read showed the remote holds.
- Bad, because the local index grows with what the store has read.

### Store nothing for a fetch whose payload isn't kept

- Good, because the local store holds only what a caller chose to keep.
- Bad, because every later put or push of fetched content uploads it again.

### Record it only for the storage API

- Good, because it fixes the case that was reported, with the smallest change.
- Bad, because revision reads, the virtual file system and prefetch keep the repeated upload, and
  readers of the same path behave differently.

## More Information

Implemented in `store_fetched` and `fetch_is_stored` in `lore-storage/src/read.rs`, with the
`no_local_write` option in `lore-storage/src/options.rs`.
