---
lep: 2026-08-02-resolved-storage-operations
title: Resolved Storage Operations for Foreign-Keyed Data
authors:
  - mattias.jansson
status: Draft
created: 2026-08-02
updated: 2026-09-01
discussion: <TBD — fill in CR link when discussion CR is opened>
---

# Resolved Storage Operations for Foreign-Keyed Data

## Summary

Lore's storage system API keeps content in a content-addressed immutable store, keyed by hash, and keeps caller-chosen names in a mutable store that maps a key to one such hash. Callers that address data by a key belonging to some other system — an asset id, a build id, a database primary key — therefore need both stores for every read and every write, and the two accesses are serially dependent: the second cannot start until the first answers. This proposal adds a pair of operations that resolve a mutable key and act on the content it names within a single request, so that addressing content by a foreign key costs the same round trip as addressing it by hash.

## Motivation

Lore's storage system API (`lore::storage`, `lore_storage_*` in [`lore-capi/lore.h`](../../lore-capi/lore.h)) exposes two keyspaces: an immutable store holding content addressed by its own hash, and a mutable store holding caller-chosen keys, each mapping to a single hash ([`lore-storage/src/mutable_store.rs`](../../lore-storage/src/mutable_store.rs)). Content addressing is what makes the immutable store deduplicating, verifiable and safely cacheable; the mutable store is what lets a caller find that content again by a name it chose.

That split serves Lore's own revision control well, because traversal is hash to hash: a revision names its tree, a tree names its children. It serves a different and increasingly common shape poorly — using Lore storage for content that another system addresses by its own identifier, such as an asset id, a build target or a source record id. Such a caller never holds the hash. Its identifier is a foreign key: meaningful to the calling system, meaningless to Lore, and stable across changes to the content it names, which is precisely what content addressing cannot express.

Serving those callers costs two network round trips per logical operation, in both directions:

- **Reading** requires `mutable_load(key)` to obtain the hash, then `get(hash)` to obtain the bytes.
- **Writing** requires `put(bytes)` to obtain the hash, then `mutable_store(key, hash)` to publish it.

The cost is not bandwidth and it is not amortizable. The second request depends on the first's answer, so the two cannot be pipelined, batched or overlapped: latency is two full round trips regardless of how much work the caller has queued behind it. A client resolving ten thousand asset ids can issue them concurrently and still pays twice the serial depth on each. Where a round trip is tens of milliseconds, that doubling decides whether a cache is viable.

The write direction carries a second cost that is not about latency. Publishing a key is safe in only one order: the content must be durably stored before the key names it, or a reader can resolve the key and then fail to read what it points at. Nothing in the API expresses that constraint, so every integration re-implements it, and each one can get it wrong independently.

Both costs fall on integrations that address content by foreign key. Callers that already hold hashes are unaffected, which is why the problem has not surfaced in Lore's own use of its storage layer.

## Goals / Non-Goals

### Goals

1. **A foreign-keyed read costs one round trip.** A caller holding a key and wanting the content it names issues one request and receives the content, at the same serial depth as a caller who already holds the hash.

2. **A foreign-keyed write costs one round trip.** A caller holding a key and content to publish under it issues one request; the content is stored and the key names it when the request completes.

3. **A key never names content the store does not hold.** The ordering constraint that integrations must currently implement themselves is enforced by the operation, on every path, including when content is large enough to be split across many fragments and one of them fails to upload.

4. **Callers can tell what actually happened.** A write reports where the content came to rest, so a caller can distinguish content that reached the remote from content that only reached the local store — a distinction the existing write path does not surface, and which determines whether other clients can see the key.

5. **Removal is expressible.** A caller can retract a key it has published, so that a foreign key whose content is no longer valid stops resolving.

6. **Both transports, one behaviour.** The operations are available over QUIC and gRPC with the same semantics, per the transport-parity property asserted in [`system-design.md` §18.1](../explanation/system-design.md).

7. **Reachable from the C API.** The operations are exposed through `lore-capi` in the same shape as the storage operations they compose, since the integrations that need them are not written in Rust. "Same shape" includes the reading options that matter at scale: a resolved read streams fragment by fragment on request, as an ordinary read does, so content size does not dictate memory.

### Non-Goals

- **A query or secondary-index facility.** These operations resolve one key to one content address. Listing keys, prefix scans, and multi-key queries are out of scope. Note that the existing `mutable_list` does not fill that gap for these keys: it reads the local store only, and the local mapping store is a cache rather than an authority, so it cannot enumerate a repository's published keys.

- **Optimistic concurrency control.** Publishing is last-writer-wins, and that is the contract rather than a default awaiting review. It is the right semantics for the caching this proposal is for: when two publishers race, either mapping is valid and neither caller is worse off for losing. Callers who do need to detect a lost update have `mutable_compare_and_swap`, which already accepts keys of this type — at the cost of the second round trip, which is the trade being made deliberately.

  Folding a compare-and-swap into `put_resolved` was considered and rejected. Goal 3 forces content to be stored before the key is published, so the swap would have to be the second step, and every failed swap would leave stored content behind — waste that grows with exactly the contention the feature would exist to handle. Inverting the order would remove the waste and break the guarantee that makes these operations worth having.

- **Cache coherence between clients.** Nothing here propagates invalidation. A client that has cached a mapping locally learns of a change when it next asks the authority, and this proposal does not add a mechanism to tell it sooner.

- **Changing the mutable store's general semantics.** The existing key types, their behaviour, and `mutable_load`/`mutable_store`/`mutable_compare_and_swap` are untouched.

## Proposed Design

### A dedicated key type

Foreign keys used this way occupy their own key type, `KeyType::Resolve` ([`lore-base/src/types/store_types.rs`](../../lore-base/src/types/store_types.rs)). Confining them to one type keeps them from colliding with the typed pointers Lore's revision layer maintains — branch tips, branch metadata, repository identity — and means the operations below never need to carry a key type on the wire, since there is only one they act on.

This addresses Goals 1 and 2 in part: the key type is implied rather than transmitted.

### Two operations

`get_resolved` takes a key and a context, resolves the key to a hash, reads the content at that hash, and returns the hash alongside the content. Returning the hash lets the caller cache the mapping and verify the payload against it, so the operation is no less verifiable than a plain `get` (Goal 1).

`put_resolved` takes a key and a fragment, stores the fragment, and publishes the key naming it (Goal 2). Storing happens first, and the key is published only once storing succeeds. For content small enough to occupy a single fragment the store and the publish are one command, so the ordering is the server's to guarantee rather than the caller's. For content split across a fragment list, the leaves upload through the ordinary write path and the key is published afterwards, gated on the whole tree having reached the remote (Goal 3).

Both operations are available on QUIC and gRPC (Goal 6) and through `lore_storage_get_resolved` and `lore_storage_put_resolved` in the C API (Goal 7). A resolved read takes the same `streaming` option an ordinary read does: without it the content is reassembled into one buffer, with it the caller receives one event per leaf fragment and peak memory follows the fragment size instead of the content size.

### File-backed variants

Callers whose content lives on disk — which is most of them, for content large enough to fragment — cannot use the two operations above without giving up what each is for. Publishing means reading the whole file into a buffer to hand across the boundary; resolving means receiving the whole content as events and writing it out again. Either way the content passes through the application's memory in full, and the streaming option only moves the problem from one side of the callback to the other.

`get_file_resolved` and `put_file_resolved` name a path instead of a buffer. Neither the caller nor Lore holds the content: a file at or below the fragment threshold is read once into the single fragment it becomes, and a larger one chunks straight off disk on the way in and is written leaf by leaf at each leaf's own offset on the way out, so peak memory follows the fragment size rather than the file size.

They keep the round trip count of the pair they mirror, including for content that fragments. A read fuses the resolve with the fetch of the *first* fragment it needs — the tree's root — exactly as `get_resolved` does, since the root is what the resolve answers with. A write fuses the publish into the upload of the *last* fragment it sends — the fragment list root, or the single fragment of content that does not fragment. Fusing at the root is safe for the reason the ordering in "Two operations" demands: the root is stored after every fragment beneath it, so each level's placement is settled before the level above is written, and a level that finds a child missing from the remote withdraws the key rather than passing it on. A tree that did not reach the remote whole therefore leaves its key unpublished, as it did when the mapping followed as a separate write. `put_resolved` publishes fragmented content the same way, so the two writes differ only in where their bytes come from.

Ranges apply as they do to `lore_storage_get_file`: a read may ask for part of the content, the file holds exactly that range from its first byte, and only the fragments the range covers are fetched. A zero-length file retracts the key, since a publish with nothing to publish is the removal described below rather than a third state.

### Reporting where content landed

Lore's existing write path treats a remote upload as best-effort: a `put` that stores locally but fails to upload reports success, leaving the fragment marked non-durable so a later write retries it. That contract is sound for `put`, and it is exactly what makes a naive key publish unsafe — a caller cannot tell from a successful write whether the content another client would need is actually there.

Writes therefore report the placement of their content: whether it reached the local store, and whether it reached the remote. For content split into fragments, the report is the intersection across every fragment and every intermediate node in the tree, so a single leaf that failed to upload is visible in the result rather than hidden behind a success (Goal 4). `put_resolved` consults that report before publishing the key remotely (Goal 3), and callers can consult it to decide whether other clients can see what they just wrote.

### Removal

A publish that carries no content retracts the key instead (Goal 5). Storing the zero hash is already how the mutable store removes a key, and a resolve that finds no mapping already reports the content as not found, so removal is the same operation with nothing to store rather than a separate verb.

### Local resolution

Reads consult the local store before the remote, on the same flags that govern every other storage read — a caller can require local, require remote, or accept either. The local mutable store acts as a cache of the mapping rather than an authority: a locally cached mapping that is absent, or that names content this store does not hold, defers to the remote, which answers the mapping and the content in the same round trip it would have spent on the mapping alone.

Because the local store is a cache, a mapping cached locally can be stale, and a caller that needs the authoritative answer asks for it explicitly. This is the same trade the existing read path makes for content, with the difference that a mutable key can move while a content hash cannot — so the choice is a caller's to make rather than a default to assume.

## Compatibility

- **Wire format** — Additive. QUIC gains two opcodes on the existing `lore-storage/0.4` ALPN; gRPC gains two rpcs and four message types in `lore.storage.v1`, plus a shared per-item status message in `lore.model.v1`. A v(N-1) peer cannot decode the new commands: over gRPC it answers `Unimplemented`, while QUIC has no such status and reports the opcode as an invalid command. A v(N) peer decodes everything a v(N-1) peer sends unchanged. No existing message's layout changes.

- **Client/server protocols** — A new client against an old server fails both operations — reported as unimplemented over gRPC, as an invalid command over QUIC — with no fallback to the two-request sequence; see Risks. An old client against a new server is unaffected, since it never sends the new commands. Authorization is unchanged: both operations run under the session authorization the existing storage commands use, and neither reads or writes anything a caller could not already reach through `get`, `put`, `mutable_load` and `mutable_store`.

- **On-disk format** — Additive. `KeyType::Resolve` is a new discriminant, and the local mutable store encodes the key type into the stored key ([`lore-storage/src/local/mutable_store.rs`](../../lore-storage/src/local/mutable_store.rs)), so the new type extends the persisted key space without disturbing existing entries. An upgraded Lore reads existing repositories unchanged; a downgraded Lore reads everything except keys of the new type, which it does not recognise and does not need.

- **CLI and public API** — Additive. Eight new `extern "C"` entry points (`lore_storage_get_resolved`, `lore_storage_put_resolved`, `lore_storage_get_file_resolved`, `lore_storage_put_file_resolved`, and their `_async` variants) and their argument and item structs. The file-backed pair adds no wire format of its own: it composes the same two commands, and is reachable across the service IPC boundary where the buffer-bearing `put_resolved` is not, since a path has a cross-process representation and a `LoreBytes` view into caller memory does not. The shared per-item completion event for writes gains two fields, appended after the existing ones: at the C level they occupy the struct's existing tail padding, so its size and every prior field offset are unchanged, and they carry `#[serde(default)]` so that the same event crossing the service IPC boundary still deserializes from a peer that predates them — the pattern `lore_complete_event_data_t` established when error detail was appended to it. No CLI subcommand changes.

## Non-Functional Considerations

- **Concurrency** — Publishes to the same key are last-writer-wins, as `mutable_store` is today. Concurrent publishes of identical *content* coalesce on the existing in-flight guard, so N publishers of the same bytes produce one upload. Concurrent resolves of one key may observe different mappings if the key moves between them, which is inherent to reading a mutable value and not introduced here.

- **Memory** — Unchanged in both directions. A publish carries a single fragment, bounded by the existing fragment size threshold, and larger content fragments through the existing write path with its streaming and backpressure intact. A resolved read offers the same `streaming` mode as `lore_storage_get`: set it and the content arrives one leaf fragment at a time, so peak memory follows the fragment size rather than the content. Left unset it reassembles into a single buffer, which is the right default for the small values this API is aimed at but not for a key naming something large. The file-backed variants bound the *caller's* memory too, which the streaming mode does not: the content never exists as a buffer on either side of the boundary.

- **Statelessness** — No new process- or library-level state. The operations reuse the existing per-session transport state and the existing local stores.

- **Determinism** — Content addressing is unchanged, so identical content produces an identical address regardless of which operation stored it. The mapping a key resolves to is by definition not deterministic across time; that is the property the mutable store exists to provide.

- **Latency** — The reason for the proposal. Foreign-keyed reads and writes drop from two serially dependent round trips to one, at every content size. A multi-fragment write pays no publish of its own either: the mapping rides on the upload of the fragment list root, which the write already had to send after its leaves.

## Migration Plan

None required. The operations are additive and opt-in: existing callers continue to use `mutable_load` + `get` and `put` + `mutable_store`, which keep working unchanged. Integrations adopt the new operations when they choose to. No data migration applies, since the new key type only holds mappings created through the new operations.

## Security Considerations

No new authority. Both operations are reachable only under the same session authorization as the storage commands they compose, are scoped to the session's repository, and can neither read nor write anything a caller could not already reach through `get`, `put`, `mutable_load` and `mutable_store`. A caller that can publish a key under the new type could already have published the same mapping through `mutable_store`, and a reader verifies content against the hash the server returned, so a poisoned mapping cannot cause a reader to accept content that does not hash to the advertised value.

The one property worth stating explicitly: the new key type is writable through the existing `mutable_store` operation as well, so the ordering guarantee described above holds for content published through `put_resolved` and not for mappings written directly. A reader must treat a resolve that finds no content as a possible outcome, which it must do anyway for a key retracted between resolve and read.

## Privacy Considerations

No implications. The operations move the same content, under the same repository scoping and the same authorization, as the operations they compose. Foreign keys are chosen by the calling system and are opaque to Lore; they are stored and transmitted exactly as any other mutable key is today, and this proposal adds no new retention, logging or telemetry of them.

## Risks and Assumptions

**Assumptions**

- **Assumption:** Foreign-keyed access is a real and growing integration shape, not a niche one — *invalidated if:* the integrations that motivated this proposal turn out to hold content hashes after all, or to tolerate the second round trip.

- **Assumption:** Latency, not bandwidth, is what limits these callers — *invalidated if:* measurement shows the second round trip is a small fraction of their end-to-end time compared with payload transfer.

- **Assumption:** One mapping per key is sufficient — *invalidated if:* callers need a key to name several content versions, at which point this becomes a versioning problem rather than a caching one.

**Risks**

- **Risk:** A new client talking to a server that predates these operations fails outright rather than falling back to the two-request sequence it could have used — *mitigation:* not addressed by this proposal. The fallback is mechanically simple and worth adding before these operations are relied on in a mixed-version deployment; it is called out in Unresolved Questions rather than assumed away.

- **Risk:** Locally cached mappings go stale, and nothing tells a client its cached key has moved — *mitigation:* accepted and documented. A caller that needs the authoritative mapping asks for it explicitly; the default prefers the local answer, as every other storage read does.

- **Risk:** The new key type accumulates entries in the local mutable store with no reclamation, since Lore's maintenance passes cover the immutable store only — *mitigation:* accepted for now, and bounded in practice by the fact that a mapping is only cached when the caller asks for caching. Reclaiming mutable-store entries is a broader gap than this proposal.

## Drawbacks

- Two more commands on each of two transports, and two more entry points in a C API that is a compatibility commitment once shipped.

- The publish is fused into whichever fragment's upload comes last, which makes the write path's placement folding load-bearing for correctness rather than only for reporting: a level that mis-reports a child as remote publishes a key naming content the server holds only part of. That is one gate in one place, and the tests that cover it are the mixed-tree ones, but it is a sharper edge than a separate mapping write had.

- Placement reporting adds a concept callers must understand to use writes correctly, where previously a successful write was simply successful. This is a truthfulness improvement rather than a new hazard, but it is new surface area.

## Alternatives Considered

### Leave the two-request sequence, and document the ordering requirement

Callers keep issuing `mutable_load` + `get` and `put` + `mutable_store`, and the documentation states the ordering constraint that a publish must follow its content.

*Rejected because:* it addresses neither of the costs in Motivation. The serial round trip remains, which is the cost that decides whether these integrations are viable, and documenting an ordering requirement still leaves every integration to implement it correctly and independently.

### Client-side batching or pipelining of the existing operations

Callers issue many key lookups concurrently, so the transport amortizes the two requests across a batch.

*Rejected because:* the two requests are serially dependent — the second needs the first's answer — so concurrency across keys does not reduce the depth for any individual key. A caller resolving ten thousand keys concurrently still waits two round trips for each. Batching addresses throughput, and the problem is latency.

### A general server-side scripting or multi-command facility

A mechanism for a client to submit a small program of dependent storage commands, of which resolve-then-read is one instance.

*Rejected because:* it is a much larger surface, with its own evaluation, resource and security questions, to solve one specific dependency that recurs often enough to deserve a first-class operation. Nothing else in the storage command set currently wants this generality.

### Store the content under its foreign key directly

Let the mutable store hold content rather than a hash, so one lookup returns bytes.

*Rejected because:* it discards content addressing for that data — deduplication, verification, and the ability to share one blob between many keys — to save a lookup. It also splits Lore's content into two stores with different durability, replication and repair behaviour.

## Prior Art

Content-addressed stores paired with a mutable naming layer are a common arrangement, and the round trip this proposal removes is a recognised cost of it.

Git separates object storage from refs, and a client fetching a branch resolves the ref and then reads the object it names; the smart transport's negotiation exists partly to avoid paying that dependency per object. Nix separates derivations from store paths and does substantial work to avoid per-path resolution latency against a binary cache. Content-addressed HTTP caches keyed by ETag face the same shape and answer it with conditional requests, which fuse the validation and the fetch into one exchange for the same reason this proposal fuses the resolve and the read.

## Unresolved Questions

- Should a client fall back to the two-request sequence when a server does not implement these operations, and should that be automatic or a caller's choice? The fallback is simple and always correct; the argument against making it automatic is that it hides a version mismatch a deployment may want to see.

- Should content published this way be reachable to the mutable store's reclamation, once such reclamation exists? Today nothing prunes mutable entries, and this proposal adds a class of them whose lifetime is governed by an external system's key space rather than Lore's own.

- Should retraction distinguish "this key never existed" from "this key was retracted"? Today both resolve to not-found, which is sufficient for a cache and insufficient for a caller that wants to detect a deliberate removal.
