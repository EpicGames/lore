---
status: proposed
date: 2026-08-03
deciders: Mattias Jansson
---

# ADR-00018: Store fragment metadata as S3 object metadata

## Context and Problem Statement

[ADR-00006](00006-s3-storage-options-reconsidered.md) decided to store a fragment together with its
payload, as a 16-byte preamble on the S3 object, so that the two could not get out of sync.
[ADR-00008](00008-aws-store-fragment-associations.md) later moved fragment/repository/context
associations into `DynamoDB`, because `ListObjects` was too slow for `query_immutable`. Following
that, the fragment itself was moved into `DynamoDB` as well: with associations already there, a
query could answer whether a fragment existed without touching S3, but reading what it was still
cost an S3 request, and moving it removed that request. No ADR records this change, and ADR-00006
has never been superseded.

The consequence is that an S3 object holds one representation of some content while a separate
`DynamoDB` record describes which representation that is. The S3 key is the *content* hash, but two
writers may legitimately hold different valid representations of the same content — LZ4 and Zstd of
the same bytes — and both address the same key. The object and the record are written independently
and are required to agree. They can fail to:

1. Two writers upload different representations to the same key and publish different fragments. The
   interleaving that leaves one writer's fragment beside the other's payload is permanent and
   requires no failure of any kind.
2. A writer replaces the object and then fails to publish its fragment. The stored fragment then
   describes bytes that are gone, and the affected partition cannot repair it, because its own
   re-put sees a full match and does nothing.

Either way the payload cannot be decompressed. It is reported as an internal size mismatch, no
amount of retrying fixes it, and nothing detects it until a read fails.

Separately, the same coupling wastes bandwidth. A put resolves to either a full match on the
address-partition-context triplet or an upload — `lookup` short-circuits to `MatchNone` when a full
match is requested, so the `MatchPartition` and `MatchHash` arms of `put` are unreachable. Content
already durable in one partition is uploaded again when another partition needs it, even though the
server has already established that the caller holds it.

## Decision Drivers

- A stored payload and the fragment describing it must not be able to disagree, under any
  interleaving of concurrent writers and any partial failure.
- Whether a hash exists must remain answerable without an S3 request. This is the reason the
  fragment was moved into `DynamoDB`, and it is not up for negotiation.
- Content already stored must be deduplicable across partitions and contexts, without a caller being
  able to claim it on the strength of a hash alone.
- Prefer correctness that follows from the storage layer's own guarantees over correctness that
  depends on a multi-step protocol being executed correctly across crashes, races and partial
  failures.
- Do not increase AWS request volume on the hot paths.

## Considered Options

- Keep the fragment in `DynamoDB` and make the pair safe with a write protocol
- Keep the fragment in `DynamoDB`, with a separate claim record marking the commit point
- Store the fragment in the S3 object body, as ADR-00006 did
- Store the fragment as S3 object metadata
- Key S3 objects by representation hash rather than content hash
- Normalize every payload to one canonical representation per hash

## Decision Outcome

Chosen option: "Store the fragment as S3 object metadata", because it is the only option that
removes the disagreement rather than policing it, while preserving the S3-free existence check that
motivated moving the fragment into `DynamoDB` in the first place.

Object metadata is part of the object version. It cannot be changed without rewriting the object, and
a `GetObject` returns headers and body from the same version. A full-object PUT is atomic, so a
reader observes the whole previous object or the whole new one and never a mix. Two writers racing
with different representations therefore both write complete, self-describing objects, and whichever
lands last is read back whole. Last-writer-wins becomes safe, which is what removes the need for a
protocol at all.

The `DynamoDB` fragment state table replaces the fragment metadata table: the presence of a row
means the hash exists, and the row additionally carries obliteration state. That keeps the existence
probe a single `GetItem` with no S3 request, and makes publishing an idempotent set-a-bit with
nothing for two writers to reconcile.

It stays a separate table from the associations table rather than being folded in as a second sort
key, even though merging would make the put probe one `BatchGetItem` instead of two `GetItem`s.
Whether a fragment metadata table is configured is what selects the read path for objects written
before this change, so its separate existence carries meaning beyond the rows it holds. Merging
would erase that signal, and would also concentrate state-row traffic onto the same partition key as
association traffic for popular hashes.

### Consequences

- Good, because a stored payload and its fragment cannot disagree. This follows from S3 object
  atomicity rather than from code being correct, so it does not degrade under crashes or races.
- Good, because it needs no S3 or `DynamoDB` feature beyond the plainest ones: `PutObject`,
  `GetObject`, `HeadObject`, `DeleteObject`, and `GetItem`, `PutItem`, `DeleteItem`, `Query`. No
  conditional S3 write, no ETag compare-and-set, no object-age heuristic, no transaction. The one
  conditional `DynamoDB` write is a create guarded by `attribute_not_exists`, whose only job is to
  avoid erasing an obliteration mark. Anything that made the pair safe while keeping them apart
  would have to reach for more than this, and would then depend on those mechanisms behaving as
  assumed under contention.
- Neutral on code size. Production code in `lore-aws` grows by roughly 460 lines, mostly the
  deduplication path and the read fallback for objects predating the change; `dynamodb.rs` is
  untouched. This is not a simplification measured in lines, and should not be argued as one.
- Good, because `get` drops a `DynamoDB` read: the fragment arrives on the `GetObject` response that
  was already being made. `copy` drops one too.
- Good, because cross-partition deduplication becomes possible at no additional request cost,
  reusing the two probe reads a put already makes.
- Good, because the bucket becomes self-describing. The fragment state table can be rebuilt from S3
  alone, and `verify` can run against the bucket without `DynamoDB`.
- Good, because `PayloadStoredDurable` is derived on read rather than persisted, so a durability
  claim cannot outlive the object it describes or be supplied by a client.
- Good, because `query` is answered from the state row and the association in parallel, with no S3
  request at all. This matters more than it sounds: `query_immutable` is called once per fragment
  stored on the ingress path and ADR-00008 names it the most frequently accessed path in the server,
  so an S3 request here would have undone the change's whole purpose.
- Neutral, because `query` consequently reports whether a payload is durable rather than what
  representation is stored, which splits one trait method into two. Reading a representation moves
  to `ImmutableStore::get_metadata`, which defaults to a `MatchFull` `query` and is overridden by
  this store to spend a `HeadObject`. That is the only path here that spends an S3 request purely
  on metadata, and it is reached by explicit metadata reads rather than by storing a fragment.
- Good, because separating the two exposed that `store_fragment` was reporting the queried fragment
  on its deduplicated path. It now reports the caller's own, with storage status merged in from the
  query — both fragments describe the same content, and only the store knows the status. That is
  the right split regardless of how any store answers.
- Bad, because objects written before the change carry no fragment metadata and need a fallback read
  from the old row, gated on configuration and retired only by an optional backfill.
- Bad, because it does not address the case where a partition still references a hash whose object
  S3 no longer has. That loss reads as an ordinary not-found, and re-putting the content does not
  repair it: the reference makes the put a no-op, and a put from another partition attaches to the
  same empty hash. The condition is detectable for free on `get` — an association plus an S3
  not-found is the entire signal — and repairable by clearing the state row, which is only safe
  because that row no longer carries a representation. Both are implemented: the loss is counted and
  logged, and the row is cleared unless an obliteration holds the mark. Reconciling the whole
  population rather than what happens to be read is left to the garbage collector.

## Pros and Cons of the Options

### Keep the fragment in `DynamoDB` and make the pair safe with a write protocol

This option was implemented in full, reviewed adversarially over several rounds, and is the most
informative of the alternatives because its difficulties are measured rather than predicted.

The protocol makes the object write-once with `If-None-Match: *`, treats the resulting HTTP 412 as
"someone else got there first", and then has to work out what that someone did. It re-reads the
metadata to see whether they published; if not, it decides from `HeadObject` whether their upload is
still in flight or was abandoned, reclaims an abandoned object with a single-winner
`If-Match: <etag>` write, and publishes through a converging conditional `PutItem` with explicit
write modes so that a losing writer stands down instead of overwriting the winner. Bounded retry
loops sit around both the put and the publish.

What that cost, concretely:

- **Reconstructing a fragment for an abandoned object by probing codecs does not work.** The natural
  repair — read the orphaned bytes, try each codec, keep the one that decompresses — was implemented
  and then disproved by execution. `decompress_into` validates the result against `size_content`, and
  Oodle takes the raw size as an *input* rather than discovering it, so a probe cannot distinguish
  "wrong codec" from "wrong size". It was replaced by reclaiming the object outright.
- **Telling an in-flight upload from a crashed one has no sound answer.** The implementation decides
  by the object's age via `Last-Modified`, which is a heuristic: it must treat a missing or
  future-dated timestamp as live, and the threshold trades a stalled writer against a merely slow
  one. Re-reading the metadata with retries narrows the window but cannot close it.
- **Convergence needs explicit write modes.** Because the row carries representation data that two
  writers can legitimately disagree about, publishing needs `Exclusive` and `Displacing` modes and
  `Superseded`/`Contended` outcomes so a race resolves to exactly one published fragment.
- **Obliteration ordering is load-bearing and easy to get wrong.** The mark must be taken before the
  association is deleted; the reverse order lets a put racing the obliteration re-associate the very
  partition being obliterated. This was introduced as a regression during development and caught by
  review rather than by tests, which is a fair indication of how legible the ordering is.
- **A crashed obliteration leaves a mark nothing clears.** The hash stays readable but unwritable,
  and resolving it requires a separate obliteration work table whose resumer must take over an
  existing mark explicitly, since the ordinary path returns early when it sees one. That table, if
  it retains completed items, also reconciles the one residue the chosen option leaves — an
  obliteration racing a put — so it is one background job rather than two. This gap is accepted
  rather than closed here.

The result works. It was verified red-green against reproductions of both corruption cases, every
guard was checked by reverting it, and it carries 96 tests. But it is roughly 2850 added lines
whose correctness rests on all of the above holding simultaneously, and one of its steps —
abandonment — is a heuristic by construction.

- Good, because it changes no storage format, needs no migration, and rolls back cleanly.
- Good, because it fixes the corruption, demonstrably.
- Good, because it can ship as a rolling deployment with a mixed fleet.
- Bad, because correctness depends on a multi-step protocol executing correctly across crashes,
  races and partial failures, indefinitely, including in code not yet written.
- Bad, because deciding whether an unpublished object is abandoned is a heuristic with no sound
  resolution.
- Bad, because it adds S3 requests on contended paths: `HeadObject` to classify, conditional writes
  that can 412 and retry.
- Bad, because the crashed-obliteration gap remains open.

### Keep the fragment in `DynamoDB`, with a separate claim record marking the commit point

Write a claim row before uploading and delete it after publishing, so a reader can tell a completed
write from an interrupted one without guessing from timestamps.

- Good, because it replaces the abandonment heuristic with a fact, closing the one hole the protocol
  option cannot.
- Bad, because it adds one `DynamoDB` write before and one delete after every put of new content,
  on the hottest write path in the server, permanently, to serve an edge case.
- Bad, because it still leaves two records describing one payload; it makes the window detectable
  rather than absent.

### Store the fragment in the S3 object body, as ADR-00006 did

- Good, because it gives exactly the same atomicity as the chosen option: one object, written whole.
- Bad, because every ranged read must offset by 16 bytes, permanently, in every caller.
- Bad, because a metadata-only read must fetch a byte range and so transfers body bytes, which is
  the cost that motivated moving the fragment into `DynamoDB`.
- Bad, because the bucket stops being raw content addressable by hash, which forecloses serving
  objects directly.
- Neutral, because ADR-00006 noted it forced an empty sentinel object per association. That no
  longer applies now associations live in `DynamoDB`.

### Store the fragment as S3 object metadata

One header, `x-amz-meta-lore-fragment: <flags>:<size_payload>:<size_content>` — for example
`8:4096:16384`. One key rather than one per field, because a key name is spelled out in full on
every request and every response, so three of them would cost several times what the values do.
Flags in hex because it is a bit field; the sizes in decimal because they are magnitudes. Plain
text rather than a packed encoding, so an object's shape is legible from `aws s3api head-object`
with no lore tooling.

Only flags that describe the payload travel there, on the test of whether they are the same answer
for every reader of those bytes. `PayloadFragmented`, the compression codec and
`PayloadRevisionState` pass it and are carried. Obliteration state does not — it changes while the
bytes do not — and stays in `DynamoDB`. `PayloadStoredDurable` does not, being a fact about which
store holds the payload, and is derived on read. `PayloadLocalCachePriority` does not either: it is
a per-machine hint about what one host should keep, so it has no meaning on a globally shared
object.

- Good, because it has the atomicity of the body preamble with none of its costs: the body stays a
  pure payload, so no offsets change.
- Good, because `HeadObject` reads it without transferring a body, so a caller that wants a
  representation with no payload can have one affordably — unlike a body preamble, which would have
  to fetch a byte range.
- Good, because the fragment rides the `GetObject` a payload read was already making, so `get` gets
  cheaper rather than more expensive.
- Neutral, because it caps at 2 KB and is US-ASCII. One short text value is far inside both, and the
  cap is the reason to spend one key rather than three on it.
- Bad, because a CDN in front of the bucket may strip `x-amz-meta-*`, which would matter if objects
  were ever served directly.

### Key S3 objects by representation hash rather than content hash

Key each object by the hash of its stored bytes, and have the `DynamoDB` row point at whichever
representation is current.

- Good, because the object becomes immutable by construction: the key determines the bytes, so no
  conditional write is needed for any reason.
- Bad, because a read must consult `DynamoDB` to learn the key before it can start the S3 request,
  turning two parallel round trips into two sequential ones on the hottest read path.
- Bad, because unreferenced representations accumulate and require garbage collection across
  billions of objects.
- Bad, because obliteration must find and delete every representation of a hash, not one key.

### Normalize every payload to one canonical representation per hash

Recompress on ingest so that hash-to-bytes is a function and all writers produce identical objects.

- Good, because concurrent writers write byte-identical objects, so the race disappears entirely.
- Good, because the fragment becomes derivable and need not be stored at all.
- Bad, because it puts recompression on the ingest path, which is CPU the server deliberately avoids
  by accepting client-compressed payloads.
- Bad, because it freezes the codec choice: changing it later means rewriting the entire population.

## More Information

The design, its behaviour under interruption and obliteration, the call-count comparison, and the
migration are set out in the accompanying proposal,
[Carry fragment metadata on the S3 object](../../proposals/2026-08-03-fragment-metadata-on-the-s3-object.md).

The `DynamoDB` write-protocol option is not hypothetical; it exists as a reviewed implementation
against the corruption it fixes. If this ADR is rejected, that is the fallback and it should ship.

This decision should be revisited if any of the following changes: objects need to be served
directly from S3 through a CDN that strips `x-amz-meta-*` headers; a metadata-only query becomes a hot path
rather than an inspection one; or S3 gains a way to make object metadata a compare-and-set target,
which would reopen options this ADR closes.

This supersedes [ADR-00006](00006-s3-storage-options-reconsidered.md), which is marked accordingly.
It agrees with that decision's principle and differs in mechanism: the fragment travels with its
payload, as object metadata rather than a body preamble.

The move of the fragment into `DynamoDB` between the two was never recorded in its own ADR. It does
not need one — the reason for it is stated above, and superseding ADR-00006 leaves a single document
describing where fragment metadata lives, how it got there, and why it is moving back.
