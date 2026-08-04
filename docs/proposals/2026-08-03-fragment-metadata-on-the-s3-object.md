---
lep: 2026-08-03-fragment-metadata-on-the-s3-object
title: Carry fragment metadata on the S3 object
authors:
  - Mattias Jansson
status: Draft
created: 2026-08-03
updated: 2026-08-04
discussion: https://github.com/EpicGames/lore/pull/157
---

# Carry fragment metadata on the S3 object

## Summary

Store the fragment describing a payload as S3 object metadata on the object holding that payload,
instead of as a `DynamoDB` record keyed by content hash. In its place a **fragment state table**
holds lifecycle state alone: the presence of a row means the hash exists, which keeps the existence
check that cross-partition deduplication depends on a single `GetItem` with no S3 request. Because
S3 object metadata is part of the object version, a reader always observes the fragment that was
written with the bytes it is reading, and the two can no longer disagree.

## Motivation

The AWS store keys S3 objects by the *content* hash while the object holds a *representation* of
that content. Two writers may legitimately hold different valid representations of the same content
— LZ4 and Zstd of the same bytes — and both address the same S3 key. The fragment describing which
representation is stored lives in a separate `DynamoDB` record, so the object and the record are
two independently written things that are required to agree.

They can fail to agree:

1. **Concurrent cross-partition writers.** Two writers upload different representations to the same
   key and publish different fragments. The interleaving that leaves writer A's fragment beside
   writer B's payload is permanent and requires no failure.
2. **A lost metadata write.** A writer replaces the object and then fails to publish its fragment.
   The published fragment now describes bytes that are gone, and the bytes that are there are
   described by nothing. A reader cannot use the payload: it decompresses with the codec the
   fragment names, which is not the codec the bytes are in, and allocates against sizes that are not
   theirs. The affected partition cannot repair it either, because its own re-put sees a full match
   and does nothing.

The result in both cases is the same: a stored payload that no reader can interpret, because the
fragment it is described by is not about it. It surfaces as an internal size mismatch or a
decompression failure, whichever the mismatched fields hit first. No amount of retrying fixes it —
the writers involved all believe they succeeded — and nothing detects it until a read fails.

Separately, the same coupling costs bandwidth that should not be spent. A put resolves to either a
full match on the address-partition-context triplet or an upload — `lookup` short-circuits to
`MatchNone` when a full match is requested, so the `MatchPartition` and `MatchHash` arms of `put`
are unreachable. Content already durable in one partition is therefore uploaded again when another
partition needs it, even though the server has already established that the caller holds it.

What is needed is threefold:

- A stored payload and the fragment describing it must be unable to disagree, under any
  interleaving of concurrent writers and any partial failure.
- Whether a hash exists must stay answerable without an S3 request. Deduplication that had to ask
  S3 would defeat its own purpose.
- Content already stored must be deduplicable across partitions and contexts, without a caller
  being able to claim it on the strength of a hash alone.

## Relationship to ADR-00006

This is not a new idea. [ADR-00006](../developing/decisions/00006-s3-storage-options-reconsidered.md)
decided in May 2024 to store the fragment together with its payload, and gave as its reason exactly
the property this proposal is after: *"the fragment metadata is stored once, meaning there are no
concerns about it getting out of sync with the stored payload."* It rejected the alternative of
duplicating metadata partly because that would risk *"client errors if the metadata flags don't
match what's stored in the payload object"* — which is the failure now occurring in production.

**ADR-00006 is still accepted and has never been superseded**, and no decision record covers the
change away from it:

- [ADR-00004](../developing/decisions/00004-s3-storage-options.md) is marked superseded by 00006.
- [ADR-00008](../developing/decisions/00008-aws-store-fragment-associations.md) introduced
  `DynamoDB`, but only for fragment/repository/context **associations**, and only because
  `ListObjects` was too slow for `query_immutable`. Its table is `hash` + `repository_context`. It
  says nothing about fragment metadata.
- No ADR describes a fragment metadata table at all.

The reason is not in the log but is not in doubt either. Per the author of both ADRs: once
ADR-00008 put associations in `DynamoDB`, a query could answer *whether* a fragment existed without
touching S3, but reading *what* it was still meant an S3 request. Moving the fragment into
`DynamoDB` as well removed that request. **S3 object metadata was not considered.**

That reasoning was sound for the options on the table, and this proposal does not dispute the goal —
it disputes that the goal requires the trade. The choice was framed as fragment-on-the-object-body,
which costs an S3 request to read metadata alone, versus fragment-in-`DynamoDB`, which costs the
guarantee that it matches the payload. Object metadata is not on that axis:

- `get` — the fragment rides the `GetObject` already being made for the payload. No extra request,
  and one fewer `DynamoDB` read than today.
- `put` — reads no fragment at all, only the state row and the association. No extra request.
- `query` — answered from the state row and the association, in parallel, with no S3 request. It is
  called once per fragment stored on the ingress path, so this is the one place where paying an S3
  request would have mattered most, and it does not.

So the S3 traffic ADR-00008's follow-on was avoiding is still avoided everywhere it mattered, while
the atomicity ADR-00006 wanted comes back. The mechanism differs from ADR-00006 in a second way that
also helps: keeping the fragment out of the body means no offset arithmetic on ranged reads. Its
noted downside — an empty sentinel object per association — is moot now that associations live in
`DynamoDB`.

[ADR-00018](../developing/decisions/00018-fragment-metadata-on-s3-objects.md) accompanies this
proposal and supersedes ADR-00006. It carries the intervening history — the move into `DynamoDB` and
why — so no separate record of that move is needed.

## Goals / Non-Goals

### Goals

- Make it structurally impossible for a stored payload and the fragment describing it to disagree.
- Keep "does this hash exist" answerable without an S3 request, so cross-partition deduplication
  stays a `DynamoDB`-only decision on the hot path.
- Need no S3 or `DynamoDB` feature beyond the plainest ones — no conditional S3 write, no ETag
  compare-and-set, no object-age heuristic, no transaction — so that correctness does not rest on
  those mechanisms behaving as assumed under contention.
- Reduce the `DynamoDB` reads on `get`.

### Non-Goals

- Changing the S3 key scheme. Objects remain keyed by the bare hex content hash.
- Changing the association table. It keeps recording which partition references which hash.
- Preventing re-upload over an obliteration tombstone. The tombstone is retained so a future policy
  can do this; this proposal deliberately allows the re-upload.
- Detecting hash collisions in the store. That belongs at lore server ingress, which verifies
  payload hashes on every ingest path.
- Reclaiming orphaned payloads. See [Garbage collection](#garbage-collection).

## Proposed Design

### Where each piece of information lives

| | Home | Why |
| --- | --- | --- |
| Payload bytes | S3 object body | — |
| `flags` (payload bits), `size_payload`, `size_content` | S3 object metadata on that object | Describes the bytes; must never be separable from them |
| Existence, obliteration state | `DynamoDB` fragment state table, PK `hash` | Must be answerable without S3; mutable, so cannot live on an immutable object |
| Partition references | `DynamoDB` fragments table, PK `hash`, SK `repository‖context` | Access control and obliteration |

The flags word is split by an explicit allowlist, `PAYLOAD_FLAGS`:

- **On the object** — `PayloadFragmented`, the `PayloadCompressed` codec group and
  `PayloadRevisionState`. These describe what the payload *is*, and are the same answer for every
  reader of those bytes.
- **In `DynamoDB`** — `PayloadObliterating`, `PayloadObliterated`. Lifecycle state that changes while
  the bytes do not.
- **Derived on read** — `PayloadStoredDurable`. This is a fact about *which store* holds the payload,
  not about the payload. The AWS store now sets it on every fragment it returns rather than
  persisting whatever a client sent, so the claim cannot outlive the object.
- **Not carried on the object** — `PayloadLocalCachePriority`. A per-machine caching hint: it says
  what one host should keep, which is not a property of the content and is not the same answer for
  every reader, so it has no business on a globally shared object.
- **Not persisted** — `PayloadDoNotReplicate`, already stripped by
  `sanitise_fragment_behavior_flags`.

The fragment is written as **one** header, `x-amz-meta-lore-fragment`, holding
`<flags>:<size_payload>:<size_content>`:

```text
x-amz-meta-lore-fragment: 8:4096:16384
```

One key rather than one per field, because a key name is spelled out in full on every request and
every response — three of them would cost several times what the values do, on every object and
every read. Flags in hex because it is a bit field and hex is how bits are read; the sizes in
decimal because they are magnitudes. Plain text rather than a packed encoding, so an object's shape
stays legible from `aws s3api head-object` with no lore tooling.

### Why this cannot tear

S3 object metadata is part of the object version. It cannot be modified without rewriting the object,
and a `GetObject` returns headers and body from the same version. A full-object PUT is atomic: a
reader observes the whole previous object or the whole new one, never a mix.

Therefore two writers racing with different representations both write complete, self-describing
objects, and whichever lands last is read back whole. Last-writer-wins is *safe*, which is what
removes the need for a protocol: no `If-None-Match`, no ETag compare-and-set, no abandonment
heuristic, no reclaim path, no convergence.

### Put

```text
              ┌──────────────────────────────────────────┐
  probe       │  GetItem association  ‖  GetItem state   │   two DynamoDB reads, in parallel
              └──────────────────────────────────────────┘
                                 │
        ┌────────────────────────┼────────────────────────┬─────────────────────┐
        │                        │                        │                     │
   state = Obliterating     row present,             row present,          no row, or
        │                   associated               not associated        Obliterated
        │                        │                        │                     │
   SLOWDOWN                     OK                 payload supplied?        payload supplied?
   (transient mark)          (nothing to do)        │           │            │         │
                                                   yes         no           yes        no
                                                    │           │            │         │
                                            PutItem assoc.   ERROR      upload path   ERROR
                                            (no S3 at all)  (payload    (below)      (payload
                                                            required)               required)
```

The upload path:

```text
  PutObject (unconditional, metadata attached)
        │
  PutItem state row, conditional on attribute_not_exists(hash)
        │
        ├── created ──────────────────► continue
        ├── exists, state = Stored ───► continue          (already published; nothing to reconcile)
        ├── exists, state = Obliterating ─► SLOWDOWN      (leave the object unassociated)
        └── exists, state = Obliterated ──► CAS Obliterated → Stored, revive
                                             (lost the CAS? already Stored is success,
                                              anything else is SLOWDOWN)
        │
  PutItem association
```

The conditional create has exactly one job: to avoid erasing an obliteration's mark. On the ordinary
"already stored" path the condition fails, and that is fine — the row holds no representation, so
the existing row is already correct and there is nothing to merge. Publishing is an idempotent
set-a-bit.

Reviving a tombstone tolerates losing its compare-and-set. Another writer reviving the same hash
produced exactly the state this one wanted, and this one's bytes are already uploaded, so failing
there would fail a put whose work is done. Only finding the hash back under an obliteration is a
reason to stop, and that is a back-off because the mark is transient. The tolerance is confined to
revival rather than put into the shared compare-and-set, which is also how an obliteration takes its
mark — accepting "already in the target state" there would let two obliterations both believe they
hold it.

`force_write` skips the probe and goes straight to the upload path, which is now trivially safe.

### Put under interruption

Every step is individually idempotent:

- **`PutObject`** — same key, same bytes, same metadata produces an identical object. A different
  representation produces a different but equally valid object. S3 never leaves a partial one.
- **State row create** — either it creates the row or it fails and reads back what is there. Both
  outcomes end at `Stored`.
- **Association write** — writing the same item again is a no-op.

So the flow needs no compensation on failure and no journal of what it was doing. Retrying a put
from the beginning converges from any interruption:

| Interrupted after | Left behind | What a retry does | Residue |
| --- | --- | --- | --- |
| probe | nothing | full put | none |
| `PutObject` | object, no row, no association | hash reads as unknown, so it re-uploads and publishes | none — the object is overwritten |
| state row | object + row, no association | probe sees `Stored` + not associated + payload, so it writes the association only | none |
| association | complete | returns `OK` | none |

**The ordering rule is load-bearing, not cosmetic.** Writing the state row before the object would
leave, on interruption, a hash that claims to exist with no bytes behind it — and that state is
*unrepairable by retry*. The next put probes, sees the row present, takes the "already stored"
branch, and writes only the association. It never uploads. Every reader of that hash then gets
not-found for content the store believes it holds, permanently. Object-before-row inverts this: the
leftover is a row-less object, which is invisible to every path and is overwritten by the next
upload.

The one case ordering cannot address is losing the object *after* the row exists — S3 durability
loss, or the obliteration race below. That is the pre-existing "published but missing payload"
condition. It is detectable (a `GetObject` not-found on a hash whose row says `Stored`) and warrants
a counter and an alarm, since the affected partition cannot repair it by re-putting.

**Orphaned objects.** Two paths leave an object nothing references: an interruption between upload
and row, and a put that uploads and then backs off because an obliteration holds the hash. Neither
is corruption — the object is well-formed and unreachable — but nothing reclaims it. This is the
same gap as [Garbage collection](#garbage-collection).

### Get

```text
  GetItem association  ‖  GetObject       one DynamoDB read, one S3 read, in parallel
        │                     │
   access check         body + metadata
                              │
                       fragment ← metadata      (same object version as the body)
                              │
                       size_payload == body.len() ?  ── no ──► ERROR (object damaged)
```

There is no `DynamoDB` metadata read on this path at all — one fewer round trip than today. The size
check is now a self-consistency check on a single object rather than a comparison between two
stores; it can only fail because the object itself is damaged, never because two records drifted.

An obliterated hash needs no special case: obliteration deletes the object, so the `GetObject`
returns not-found.

### Query

```text
  GetItem association  ‖  GetItem state          no S3 request, ever
        │                      │
   match_made            Stored ──► match, PayloadStoredDurable derived
                         obliteration ──► miss
                         no row ──► miss
```

**Query must not reach S3.** `query_match_full` in `lore-storage`'s write path calls it once per
fragment stored, and ADR-00008 identifies `query_immutable` as the most frequently accessed code
path in the server. A `HeadObject` here would put an S3 request behind every already-present fragment
of a push — the exact load this proposal exists to remove. The two probe reads run in parallel and
answer on their own.

The consequence is that `query` reports **whether a payload is there and durable, not what
representation is stored**: the returned fragment carries the derived `PayloadStoredDurable` and no
sizes. That is everything the ingress path reads — `stored_flags` looks only at flags — and it means
an object predating the cut-over needs no fallback either, since `query` never wants its
representation.

Two callers are left owing something by this, and neither is resolved here:

- Reading a representation without its payload needs its own way to ask, which is where the
  `HeadObject` belongs — one request, on the one path that genuinely wants one. **Implemented** as
  `ImmutableStore::get_metadata(partition, address)`, defaulting to a `MatchFull` `query` so every
  store whose `query` already reports the representation is unaffected, and overridden by the AWS
  store to add the `HeadObject`. Two callers switched to it: the `lore_storage_get_metadata`
  command, and the server side of the remote metadata op in
  `lore-server/src/protocol/storage/get.rs` — worth noting, because that second one means a remote
  client's `GetMetadata` would otherwise have received sizeless fragments over the wire, not just a
  local inspection command. Covered by `get_metadata_returns_the_representation_that_was_stored`,
  `get_metadata_reads_a_preexisting_object_from_the_fragment_metadata_table` and
  `get_metadata_reports_a_miss_for_an_unknown_address`.
- `store_fragment` returned `query.fragment` as `StoreResult.fragment` on the deduplicated path,
  which under this design would have reported zeroed sizes. **Fixed**: it now reports the caller's
  own fragment, with the storage-status flags taken from the query. That split is the correct one
  either way. Both fragments describe the same content — the address matched on its hash — so they
  agree on `size_content`, and where they differ it is only over which representation happens to be
  stored, which is not what the caller asked. Storage status is the opposite: it is the store's
  answer and the caller cannot know it, which is the entire point of the short circuit.

### Obliterate

```text
  read state ── absent, or already an obliteration ──► OK (idempotent)
        │
  CAS state Stored → Obliterating          take the mark, before touching the association
        │
  DeleteItem association                   ← the compliance obligation is discharged here
        │
  drain: wait max(dynamodb timeout, 100ms) ← let an in-flight put land its association
        │
  count associations for the hash
        │
        ├── any remain ──► CAS Obliterating → Stored, done  (payload survives; hash stays writable)
        │
        └── none ──► obliterate sub-fragments (if fragmented)
                     DeleteObject
                     CAS Obliterating → Obliterated         (tombstone)
```

The mark is taken *before* the association is deleted. The other order would let a put racing the
obliteration re-associate the very partition being obliterated.

Sub-fragment recursion moved after the reference count, so it is skipped entirely when other
partitions still hold references. The parent payload is still present at that point, so its
reference list can be read.

#### Put racing obliteration

Because the mark is taken first, the two flows can interleave in exactly three ways:

1. **Put probes before the mark, uploads after it.** Its conditional create fails, it reads back
   `Obliterating`, and it returns `SLOWDOWN` *without writing an association*. Compliance is
   unaffected. The uploaded object is left unreferenced, and if the obliteration has already deleted
   the payload, that upload resurrects it as an orphan.
2. **Put probes after the mark.** It sees `Obliterating` at the probe and backs off having done
   nothing.
3. **Put passed its probe and races the association delete.** This is what the drain is for. If the
   put's association lands before the count, it is counted, the mark is released, and the payload
   survives — correct. If it lands after the count, the obliteration deletes the payload and
   tombstones the hash, leaving that partition with an association to a hash whose object is gone.

Case 3's residue is a narrowed window, not a closed one, and it is worth being explicit that no
drain length closes it — nothing crashed, so nothing can be resumed; two correct operations simply
interleaved badly. It is self-healing on the next put from that partition, which sees `Obliterated`,
falls through to the upload path, revives the hash and re-associates. Reads before that fail as
not-found, and the [obliteration work table](#the-obliteration-work-table) is what finds and repairs
it without waiting for someone to notice.

#### Obliteration under interruption

The compliance obligation — removing the obliterated partition's reference — is discharged by a
single atomic `DeleteItem`. Every step after it is cleanup. An obliteration interrupted at any point
past that `DeleteItem` has therefore already met the requirement, and re-running it is safe: the
state read at the top returns early for a hash already marked or tombstoned.

Interruption *before* the `DeleteItem` but after the mark is the one bad case: the mark is left set
with nothing to clear it. The hash stays readable — the object is untouched until after the count —
but becomes unwritable, since every put now returns `SLOWDOWN`. This is a known gap, and it is the
first of the three duties of the [obliteration work table](#the-obliteration-work-table) below. Note
that resuming an obliteration requires taking over an existing mark explicitly, because `obliterate`
deliberately returns early when it finds one.

### The obliteration work table

Recording each obliteration as a work item, and **keeping the item after the obliteration completes**,
gives one background job three duties on one schedule:

| Item | State found | Duty |
| --- | --- | --- |
| incomplete, stale | a mark with nothing advancing it | **resume** — the crashed-obliteration case above |
| complete | `Obliterated` **and** associations remain | **reconcile** — the drain race in case 3 |
| complete | no associations remain | drop the item |

The reconcile predicate is exact rather than heuristic. A successful re-put moves the state back to
`Stored` before it associates, so a tombstone coexisting with a reference means no revival happened
and something is pointing at a payload that was destroyed. Detecting it costs one association count
and one state read per completed item.

It runs on a longer timer than the resume pass, and that delay is load-bearing rather than merely
cautious: a legitimate revival is upload, then advance state, then associate, so sampling mid-sequence
could momentarily observe `Obliterated` alongside the racing put's association. Waiting well past any
in-flight put makes the observation stable.

The reconcile pass can repair rather than only report, and this is the one repair in the design that
is unconditionally safe: the payload is *provably* gone, because only the obliteration that wrote
that state could have deleted it, so the association references nothing and removing it destroys
nothing. It is also the fix that matters. The tombstone alone does not block recovery — a put over
`Obliterated` already takes the upload path — but the dangling association makes `query` report a
match, so the client believes it holds the content and never re-sends it. Clearing the association
makes the next query a miss and the content comes back.

Retention of completed items is therefore the mechanism, not bookkeeping: they must outlive the
reconcile window.

This is bounded work, which is what makes it affordable. It walks obliterations, not objects.

### When the payload is gone but the reference remains

This is the one failure the design does not remove, and it is worth stating plainly because it is
both silent and self-perpetuating.

**The condition.** A partition still references a hash and S3 no longer has the object. It arises
from an obliteration interrupted between `DeleteObject` and the tombstone; from case 3 above, where
a racing put's association lands after the reference count reached zero; from an out-of-band
deletion; or from an S3 durability event.

**Why it is silent.** `get` maps `NoSuchKey` to `AddressNotFound`, which is exactly what a hash that
was never stored returns. Nothing separates "we never had this" from "we had this and lost it", so
the loss surfaces as an ordinary miss and is invisible in aggregate.

**Why it is self-perpetuating.** The obvious repair — have the client send the content again — is
precisely the path deduplication short-circuits. A re-put from the same partition probes, finds the
state row and its own association, and returns `OK` without uploading. A put from a *different*
partition holding the payload takes the deduplicate branch and writes an association, attaching a
second partition to a hash with no bytes. Re-putting therefore does not repair it, and can spread
it.

This is not new. The same shape exists on `main`: a published metadata row with no object, and a
re-put that sees `MatchFull` and does nothing.

**What can be done**, in increasing order of commitment:

1. **Detect it.** An association that survived the access check, plus a not-found from S3, *is* the
   condition — the read holds both facts already and needs no extra request to notice. S3 not-found
   is definitive rather than transient, so there is nothing ambiguous to resolve. A counter and an
   `error!` turn a silent miss into an alarmable one. *Implemented, on both reads that can observe
   it: `get` and `get_metadata`. Doing it on only one would have made detection depend on which call
   a client happened to make, and `get_metadata` is the cheaper one. The counter is
   `store.immutable.missing_payload`.*

   This is also why a failed `DynamoDB` read must never be reported as not-found. Repair acts on
   that signal, so mapping a throttle to a miss would let overload be recorded as data loss and
   clear a state row. See `is_dynamodb_overloaded`, which sorts timeouts, dispatch failures,
   429s, 5xxs and throttling codes from real errors. Covered by
   `a_failed_fragment_metadata_read_does_not_clear_the_state_row`.
2. **Make it repairable, by clearing the state row on detection.** With the row gone the next put
   finds no state, takes the upload path, and republishes. This is only safe because the row carries
   no representation: deleting it discards nothing that the next write does not re-derive, which
   would not have been true when the row held the fragment. The cost is a write on a read path, so
   it wants to be a deliberate choice rather than a side effect. *Implemented, and skipped when an
   obliteration holds the mark: removing a mark mid-obliteration would let a put republish
   underneath it. Covered by `a_read_of_a_lost_payload_clears_its_state_so_a_put_can_restore_it` and
   `a_read_of_a_lost_payload_leaves_an_obliteration_mark_alone`, and
   `a_failed_repair_still_reports_the_lost_payload_as_not_found` for the best-effort part.*
3. **Verify on the deduplicate branch.** A `HeadObject` before associating would stop the spread,
   and is rejected — it puts an S3 request on the hot path to defend against a rare condition, which
   is the trade this proposal exists to avoid.
4. **Repair out of band.** For losses caused by an obliteration racing a put, the
   [obliteration work table](#the-obliteration-work-table) finds every instance rather than only the
   ones somebody happened to read, and does it by walking obliterations rather than objects. For
   losses with any other cause — an S3 durability event, an out-of-band delete — nothing bounds the
   search but the population itself, so that needs a sweep over S3 Inventory, which is the same
   machinery as [Garbage collection](#garbage-collection) walked in the opposite direction and
   belongs with it.

1 and 2 are implemented; 4 is left to the obliteration work table and the GC sweeper. Both repair steps are best effort — a failure
to clear the row leaves the hash exactly as it already was, and the alarm has been raised regardless.
One residue remains: an obliteration that takes the mark between the state read and the delete has
its mark cleared, so its later compare-and-set fails and logs. That is narrow, and it loses cleanup
rather than corrupting anything.

### Garbage collection

Compliance requires only that the obliterated partition's reference is gone, which the
`DeleteItem` above achieves. Deleting the payload is therefore garbage collection, not compliance —
it is only correct to do when the reference count reaches zero.

This proposal keeps that reclamation inline, as it is today. It is worth separating later: a
background sweeper over zero-reference hashes can afford to be slow and conservative in a way that a
synchronous obliterate cannot, and it shares a schedule with the
[obliteration work table](#the-obliteration-work-table).

### Call-count comparison

| Operation | `main` | This proposal |
| --- | --- | --- |
| Put, new content | 1–3 DDB reads, 1 S3 PUT, 2 DDB writes | 2 DDB reads (parallel), 1 S3 PUT, 2 DDB writes |
| Put, already associated | 1–3 DDB reads | 2 DDB reads (parallel) |
| Put, dedup across partitions | not supported — re-uploads | 2 DDB reads, 1 DDB write, **no S3** |
| Get | 2 DDB reads, 1 S3 GET | **1 DDB read**, 1 S3 GET (parallel) |
| Query (per fragment, ingress) | 2 DDB reads | 2 DDB reads (parallel), **no S3** |
| Copy | 2 DDB reads, 1 DDB write | **1 DDB read**, 1 DDB write |

Cross-partition deduplication is new here, and costs no additional request: it reuses the same two
probe reads the put already makes.

## Compatibility

- **Wire format** — N/A. `Fragment` is unchanged on the wire and in the public API.
- **Client/server protocols** — N/A.
- **On-disk format** — Changed for the AWS store. S3 objects gain fragment metadata, and the rows keyed
  by hash change from a flattened `flags`/`size_payload`/`size_content` fragment to a single `state`
  attribute. The key schema is unchanged, so no table needs to be recreated — the fragment state
  table can be, and by default is, the same physical table that held fragment metadata, with the two
  row shapes distinguished by shape. See [Migration Plan](#migration-plan).
- **CLI and public API** — Which `DynamoDB` tables are used stays operator configuration and none of
  those values change; the tables keep their identities and only what they hold differs. Two keys:
  - `dynamodb_fragment_metadata_table`, optional, naming the table to read fragments from for
    objects predating the cut-over. It **accepts `dynamodb_metadata_table` as an alias**, because
    that is already what an existing configuration points at — a table holding fragment metadata,
    which is exactly what it is read for afterwards.
  - `dynamodb_fragment_state_table`, **required, and deliberately without an alias**. The role is
    new, so which table serves it is a decision an operator makes rather than inherits. A
    configuration that names neither fails to start rather than silently reusing whichever table
    used to hold fragment metadata.

  So an existing configuration must add one line, and adds it knowingly. In a migrating deployment
  both keys normally name the same table, since the two row shapes share it and are told apart by
  shape.

## Non-Functional Considerations

- **Concurrency** — The central claim. Concurrent writers with different representations converge on
  whichever object landed last, always coherent, with no coordination. The only remaining ordered
  step is object-before-row on the put path, and the only remaining compare-and-set is on the
  obliteration state, which holds a single attribute. Covered by
  `concurrent_writers_cannot_tear_the_fragment_from_its_payload`, which runs four writers with four
  different representations over 64 randomized rounds and asserts the stored fragment describes the
  stored bytes. The put flow is idempotent step by step, so it needs no journal and no compensation;
  see [Put under interruption](#put-under-interruption). The one place that still needs a timing
  window is the obliteration drain, and its residual race is self-healing rather than corrupting.
- **Memory** — Unchanged. Metadata travels in HTTP headers, not in the payload buffer, so the read
  path allocates exactly the payload.
- **Statelessness** — Improved. `PayloadStoredDurable` is derived per store rather than persisted,
  so no server writes down a durability claim another server will serve.
- **Determinism** — Deduplication decisions are a pure function of the two probed rows.

## Migration Plan

The change is not backward compatible at the storage layer, and the population is large enough that
rewriting it is not an option: objects written before this change carry no fragment metadata, and their
fragments live in table rows this code no longer reads.

### Prerequisite: no prefix-era objects

lore once stored the fragment as a 16-byte `Fragment` prefix on the object body — the layout
[ADR-00006](../developing/decisions/00006-s3-storage-options-reconsidered.md) chose — and `main`
still carries a compatibility branch for it in `read_payload`.

**This plan requires that no such objects remain in any live deployment**, which is the case: the
prefix layout has no surviving deployments. That is a prerequisite rather than a conclusion, and it
is what reduces the migration to two eras instead of three. It must be re-checked before Phase 1
ships, because a reader under this plan interprets a bare object body as the whole payload — a
prefix-era object would silently yield the payload with 16 bytes of `Fragment` glued to its front,
sized wrong and failing to decompress.

Two consequences follow. The dead compatibility branch in `read_payload` should simply be deleted
rather than repaired — it never worked anyway, since it captures `buffer_size` before stripping the
prefix and then compares the unstripped length against `size_payload`, so it always returns a size
mismatch. And no phase below needs a third branch.

Both remaining eras are therefore:

1. **Table era** (current `main`) — bare object body, fragment in the metadata table row.
2. **Object-metadata era** (this proposal) — bare object body, fragment in the object's user
   metadata, `state` in the row.

### Prerequisite: no mixed fleet

**The rollout is a full stop followed by a full start.** Every server running the current code is
taken down before any server running the new code comes up; the two versions are never alive
together.

This is what collapses the migration to a single release. The usual dual-read-then-dual-write
staging exists only to keep a mixed fleet mutually intelligible: old servers must be able to read
what new servers write, which forces new servers to keep writing the fragment to the table row, and
that in turn means still publishing two records — so the tearing guarantee would not arrive until
the final phase. With no old server alive to satisfy, none of that is needed. **Nothing is ever
written to the old row shape, and the tearing guarantee holds from the first write after start-up.**

The cost is the downtime and the loss of a rolling rollback: see [Rollback](#rollback).

### Phase 1 — Cut over

Deploy this proposal's code to the stopped fleet and start it. New content is written with the
fragment on the object and `state` in the row.

The existing population is not rewritten, so this single release must read both eras. Two pieces of
read compatibility are required, and both are cheap:

- **Fragment.** Two functions parse a fragment — `load`, reached from `get` and from obliteration's
  sub-fragment recursion, and `head_fragment`, reached from `query`. The fallback belongs inside
  those two, and every caller inherits it. `put` reads no fragment, only the state row and the
  association, and `copy` resolves through `lookup` alone, so neither needs anything.

  The trigger is `ObjectMetadataError::Absent` specifically — the object came back intact and
  carries no lore metadata, which is exactly "this object predates the cut-over". On `Absent`, read
  the fragment from the table row, which is still present and still authoritative for that object.
  `Malformed` must **not** fall back: metadata that is present but unparsable means a damaged
  object, and describing it from a table row would reintroduce the very mismatch this proposal
  removes. A missing object stays a plain `AddressNotFound`.

  The discriminator costs no extra request — it is the absence of headers on a response already
  received. The fallback itself adds one sequential `GetItem` on a legacy `get`: the same three
  requests `main` makes, serialized rather than parallel, and only for pre-cut-over objects.
  Issuing that read speculatively in parallel would restore the latency at the price of a
  `DynamoDB` read on every `get` for the whole transition, which is the wrong trade.

  **The fallback is gated on configuration**, by the new optional
  `dynamodb_fragment_metadata_table` setting. A deployment that has stored objects the old way
  sets it — normally to the same value as `fragment_state_table_name`, since both row shapes live in that
  table and are told apart by shape. A deployment created after the cut-over leaves it unset, and
  then never issues the fallback read at all: it has declared that no object without metadata was
  ever written, so one turning up is damaged rather than old, and is reported as such instead of
  being described from a row that cannot be about it. The setting is also the migration's own
  end-state marker — once a backfill completes, removing it retires the fallback.

  *Implemented, and covered by `get_falls_back_to_the_legacy_row_for_an_object_with_no_metadata`,
  `query_matches_a_preexisting_object_without_reading_s3`,
  `get_refuses_an_object_with_no_metadata_when_no_legacy_table_is_configured` and
  `get_does_not_fall_back_for_an_object_with_damaged_metadata`.*
- **State.** None. The fragment state table is its own table and starts empty, so nothing reads a
  table-era row as state and no tolerance is needed for their shape. A tombstone in the old table is
  likewise invisible, which is only acceptable because no obliterated fragment exists there — if one
  ever did, its hash would read as absent and become re-uploadable.

**Nothing is migrated ahead of time; the population migrates itself as it is used.** The fragment
state table starts empty and is its own table, so a hash stored before the cut-over has no state row
and the put probe finds nothing. That is the intended path, not a gap: the put takes the upload
branch, writes the object with its own metadata, creates the state row as `Stored`, and associates.
The hash is fully migrated from then on, and every later put deduplicates against it.

So migration is lazy and driven by traffic. Hot content converts on first write after cut-over; cold
content converts whenever it is next written, or never — and never is fine, because the fallback
read keeps it readable for as long as the fragment metadata table is configured. There is no
backfill job, no window to coordinate, and no ordering requirement between deployment and data.

The cost is bandwidth: content that is already durable is uploaded once more, the first time a
client puts it after the cut-over. That is bounded, it happens at most once per hash, and it buys a
self-describing object.

### Phase 2 — Backfill (optional)

The fallback can remain forever at the cost of carrying the dual-read branch. To retire it, backfill
object metadata for the existing population.

`CopyObject` with `MetadataDirective=REPLACE` rewrites an object's metadata without transferring the
body through the caller, taking the fragment from the table-era row. It is a per-object S3 request
against billions of objects, so it should be driven from an S3 Inventory manifest as a rate-limited
background job, and it is only worth doing if the dual-read branch proves to be a real burden.

An important interaction: `CopyObject` replaces the object version, so a backfill racing a
concurrent put could reinstate an older representation. Backfill must therefore either take the
obliteration-style mark for the hash, or condition the copy on the source ETag it read.

### Rollback

Collapsing the rollout buys the tearing guarantee immediately and pays for it here: **there is no
clean rollback once the fleet has served writes.** Content written after cut-over is described only
on its object, so the previous build — which reads the fragment exclusively from the table row —
cannot read any of it. Reverting would strand every object written since start-up.

Two options, to be decided before the cut-over rather than during an incident:

- **Roll forward only.** Accept that the previous build is not a rollback target once writes begin,
  and treat any defect as fix-forward. This is the simpler position and is defensible given the
  narrow blast radius: the storage-layer change is confined to `lore-aws`, and the pre-cut-over
  population stays readable throughout.
- **Prepare a rollback build.** Keep a branch of the current code carrying only the dual-read
  fragment fallback. It writes the old shape but reads both, so it can be deployed over a cut-over
  fleet without stranding anything. This costs one extra build to maintain and validate, and it must
  exist *before* the cut-over to be worth anything.

Either way the window is bounded by how long the fleet serves writes before the change is trusted,
so a short soak with writes disabled — reads only, exercising the dual-read path against the
existing population — is worth doing first.

## Security Considerations

Cross-partition payload deduplication remains gated on the caller supplying the bytes. A caller that
presents only a hash is refused (`Payload buffer required`) even when the payload is already
durable, because a hash on its own is not evidence the caller holds the content; treating it as such
would let any known hash be attached to a partition that never had it. This is a property of the
deduplication mechanism and cannot be enforced at ingress, which does not know whether the upload
will be skipped. It is covered by
`put_without_a_payload_may_not_claim_stored_content`.

Deriving `PayloadStoredDurable` rather than persisting it closes a smaller gap: a persisted
durability flag could previously be supplied by a client and would survive the object it described.

Obliteration's compliance obligation — removing the obliterated partition's reference — is
discharged by a single `DeleteItem` that no longer depends on any subsequent step succeeding.

S3 object metadata is not covered by the object ETag, so it cannot be used as a compare-and-set
target. Nothing in this design does so.

## Resolved

- **Which flags travel on the object.** `PayloadRevisionState` does: it says what the payload *is*,
  which is the same answer for every reader of those bytes. `PayloadLocalCachePriority` does not: it
  is a per-machine hint about what one host should keep, so it has no meaning on a globally shared
  object.
- **The two `DynamoDB` tables stay separate.** Merging them — `PK=hash, SK=""` for state and
  `SK=repository‖context` for associations — would turn the put probe into one `BatchGetItem`
  instead of two `GetItem`s. It is rejected because the fragment metadata table's separate
  existence is doing load-bearing work beyond holding rows: whether it is configured is what selects
  the read path for objects predating the cut-over. Folding it into the associations table would
  erase that signal, and would also concentrate state-row traffic onto the same partition key as
  association traffic for popular hashes.
