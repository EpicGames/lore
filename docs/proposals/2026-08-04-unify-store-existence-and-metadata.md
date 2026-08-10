---
lep: 2026-08-04-unify-store-existence-and-metadata
title: Unify store existence, query and metadata
authors:
  - Mattias Jansson
status: Draft
created: 2026-08-04
updated: 2026-08-07
---

# Unify store existence, query and metadata

## Summary

Replace `exist`, `exist_batch`, `query` and `get_metadata` on `ImmutableStore` with two operations:
a batched resolution reporting the best match a store can establish for each address, and a metadata
read. Neither takes a `StoreMatch`, and neither does `get`, so the match ladder becomes something a
store reports rather than something a caller requests, and read scope becomes store configuration
rather than a value threaded through every call and duplicated by a process flag. A written contract
binds every store and every protocol carrying an answer, enforced by one battery they all run. The
immediate result is a store layer that answers one question one way; the step it exists to enable,
taken separately, is a push that duplicates an association with a copy instead of transferring a
payload the repository already holds.

## Motivation

`ImmutableStore` has four ways to ask whether a store holds an address:

```rust
async fn exist(partition, address, match_requested) -> StoreMatch;
async fn exist_batch(partition, &[Address], match_requested) -> Vec<StoreMatch>;
async fn query(partition, address, match_requested) -> StoreQueryResult;
async fn get_metadata(partition, address) -> StoreQueryResult;
```

They are not four questions. `exist` is `query` with the fragment discarded, `get_metadata` is
`exist` with the fragment fetched, and `exist_batch` is `exist` over a slice. Eighteen types
implement the trait, so each is written eighteen times — four places per store to answer one
question differently.

### The stores disagree

**AWS** resolves the same address two ways: `exist` reads the association table alone, `query` reads
it *and* the fragment state table and refuses obliterated content. Only one knows what obliteration
is. They also disagree about what the returned level means — `exist` echoes back the level it was
handed, `query` reports the level found, and the trait says the latter. The divergence is known and
worked around: the replication handler routes single-address requests through `exist` and
multi-address through `exist_batch`, commented as compensating for "subtle differences between
`exists` and `exists_batch`".

**The replicas** send one request twice. `Query` is `pub struct Query(pub ExistsBatch)` — one
payload, two response shapes — so `exist` and `query` are two code paths, two sets of labels and two
client methods issuing the same bytes. Both define `get_metadata` as `query(MatchFull)`, which
answers with durability instead of a representation: the caller receives a fragment with no sizes.

**The remote store** downloads the whole payload to answer a metadata question, then discards the
bytes.

**The composite** implements the same local-then-durable-and-replicas fan-out five times — same
delay, same cancellation token, same accounting — close to four hundred lines, three of them
answering existence and a fourth describing what it found. Each takes the winning store's answer
whole, so one store's report about local residency can be served as if it were another's.

**The local store** already has one operation, `find()`. The four methods are four holes onto it.

**The gRPC replica** supports none of them, and says so four times.

### The parameter is not shaped like the question

`StoreMatch` has four levels, and as a *request* only two are ever used: almost every call site
passes `MatchFull`, a handful pass `MatchHash`, and no caller anywhere passes `MatchPartition` — the
level AWS spends a distinct code path on and the wire reserves a status byte for.

It cannot express what callers want. A caller does not want to know whether there is a match at
level N; it wants to know how good a match there is, so it can decide what work to skip: the upload,
the compression, the transfer. Asking at a level and receiving that level back, as AWS does, returns
nothing the caller did not already supply.

### The upload path cannot see what it needs

The decision the level exists to inform is on the write path: given content to store, is this a
payload that must be transferred, or one the repository already holds under another context, needing
only an association? `MatchPartition` is that question answered, and `copy` — duplication without
payload transfer — is already on the trait and on the wire.

The upload path cannot ask. `get` returns bytes and no level, so a caller that loads content learns
nothing about whose association it read. Existence answers do carry a level, but by the time one
reaches the writer it has been flattened: the caller asked at `MatchFull`, so a partition match
comes back as a miss. Push therefore loads the payload and puts it with the payload unconditionally,
and never asks the remote what it holds — it could not use the answer, because what comes back
depends on which store backs the server and arrives a level below what was sent.

The level alone would not be enough. A copy names a source partition, and the answer says only
*that* a better match exists, not where, so a client whose cache holds the content under some other
partition it also has access to cannot name the source — even when it holds a token proving it may
read from there.

The facts needed to choose copy over put exist in the stores, and every layer between there and the
writer discards them.

### `get` carries the ladder, and duplicates a policy

`get` takes a `match_required` that every call site passes `MatchFull` for, except two passing
`MatchHash`. Those two express not a level but a policy: may this read be satisfied by bytes stored
under another partition?

That policy already exists separately, as the process-wide `LOCAL_ISOLATION` read into
`ReadOptions::isolate`. Having both, the read path spends two round trips asking what the store is
already configured to decide, and the answer lives in a process global that every store in the
process shares whether or not it serves the same callers.

### Nothing states what the answers mean

The trait's doc comments describe mechanics and say nothing about obligations: whether a returned
level is the one found or the one asked for, whether obliterated content may match, whether a store
may answer less precisely than it knows, whether a caller may read a weak answer as absence. Each of
the eighteen implementations answered for itself, which is why they answer differently, and no test
asserts any of it across stores.

The gap extends past the trait. A handler mapping a level onto a status byte decides what may cross
a trust boundary, and a store reached through it must obey the same rules as one called in process,
or a caller's guarantees change with the deployment.

### What is needed

- One answer per store, so two callers asking the same thing cannot get different answers by picking
  different methods.
- A contract binding every store **and every protocol that carries an answer**, enforced by tests
  every implementation runs rather than by each implementation's reading of a doc comment.
- Obliterated content must never match, anywhere, through any path. It is a compliance property that
  currently holds only where state happens to be consulted.
- A store must be free to answer less precisely than the truth when precision is expensive, without
  any caller mistaking that for absence.
- The trust-boundary rule stated and enforced in one place.
- A caller must be able to learn that its own partition already holds the content, so it can
  duplicate an association instead of transferring bytes. That fact exists in the stores today and
  is discarded on the way out: by a parameter that asks at a level instead of asking what is there,
  by an implementation that answers with the level it was asked, and by a wire that demotes it in
  transit.
- Read scope must be a property of how a store is configured, not a value threaded through every
  call and duplicated by a second mechanism.

## Goals / Non-Goals

### Goals

- Reduce the four existence-shaped trait methods to two, and remove `StoreMatch` as an input.
- Make obliterated content unmatchable through every path in every store.
- Give the trait a written contract, binding stores and the protocols that carry their answers
  alike, about what a reported level means — so under-reporting is legal and over-reporting is a
  defect — and enforce it with one test battery every implementation runs.
- Stop the store allocating a result vector per existence call, and let the caller own the buffer.
- Collapse the duplicated fan-out in the composite store and the duplicated request paths in the
  replica stores.
- Move cross-partition read scope into store configuration and delete the parallel mechanism.

### Non-Goals

- Changing the client wire format. The status byte stays as it is; only its naming and its decoding
  are corrected.
- Changing what deduplication is permitted to do across partitions. That was settled by
  [LEP 2026-08-03](2026-08-03-fragment-metadata-on-the-s3-object.md).
- Deciding whether a put may re-upload over an obliterated hash. Making the answer consistent is in
  scope; the policy is not.
- Reworking `put`, `copy`, `obliterate`, eviction or garbage collection.
- Changing push to duplicate an association with a copy on a partition match. That is the step this
  work is for, and it is deliberately a separate change: a caller-side decision that becomes safe
  only once the level it reads means the same thing from every store and survives the wire intact.
- Giving callers a way to ask for content held under any context in a partition. Nothing expressible
  today is lost, since an isolating store has always read the exact association. Saying "any
  reference in this partition" needs a signal in the request — a zero context is the candidate — and
  it pairs with loosening `copy`, which demands an exact source context and so cannot act on a
  partition match at all. Both belong with the push change above.

## Proposed Design

### The trait

```rust
/// What a store found for one address, and the payload if one was asked for.
#[derive(Clone, Default)]
pub struct StoreGetData {
    /// The representation as stored: compression and sizes, not the content they decode to.
    pub fragment: Fragment,
    /// The level the lookup resolved at, gated by `read_scope`. Absence is this or an error,
    /// so a caller tests against `MatchNone` rather than requiring `MatchFull`.
    pub match_made: StoreMatch,
    /// The partition the content was found in. Zero when nothing matched.
    pub partition: Partition,
    /// The bytes, when they were asked for. `get_metadata` never carries them.
    pub payload: Option<Bytes>,
}

/// What a store holds for one address.
#[derive(Clone, Copy, Default)]
pub struct StoreMatchResult {
    /// The highest level the store established. A lower bound — see the contract.
    pub match_made: StoreMatch,
    /// The partition the content was found in. Zero when nothing matched.
    pub partition: Partition,
    /// Whether the payload is held locally.
    pub stored_local: bool,
    /// Whether the payload is durable.
    pub stored_durable: bool,
}

#[async_trait]
pub trait ImmutableStore {
    /// How widely this store searches when serving content: `MatchFull` where it isolates
    /// partitions, `MatchHash` where it does not. One policy for both reads, so they cannot
    /// drift into disagreeing about what is reachable.
    fn read_scope(&self) -> StoreMatch;

    /// How widely it searches when reporting what it holds, which is wider: `MatchPartition`
    /// where it isolates partitions, `MatchHash` where it does not.
    fn query_scope(&self) -> StoreMatch;

    /// Resolve addresses against this store, writing one result per address into `results` in
    /// input order, at `query_scope`. `results` must be the same length as `addresses`.
    async fn query(
        self: Arc<Self>,
        partition: Partition,
        addresses: &[Address],
        results: &mut [StoreMatchResult],
    ) -> Result<(), StoreError>;

    /// The representation, searched at `read_scope`. Carries no payload.
    async fn get_metadata(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError>;

    /// The representation and the payload, searched at `read_scope`.
    async fn get(
        self: Arc<Self>,
        partition: Partition,
        address: Address,
    ) -> Result<StoreGetData, StoreError>;

    …
}
```

`exist`, `exist_batch` and the old `query` are removed; the batched resolution takes the `query`
name. `StoreMatch` survives as a return value only — nothing requests a level any more.

`get` and `get_metadata` answer with one type, because they are one lookup differing only in whether
the bytes are fetched. `StoreQueryResult` — named after an operation that no longer returns it —
becomes `StoreGetData` and gains the optional payload. Both reads therefore report the level they
matched at, which is what lets a writer holding content choose between duplicating an association
and transferring a payload.

Results are written into a caller-owned slice rather than returned in a `Vec`. Every `exist_batch`
today allocates one — the local store pushes into a `vec![]`, the AWS store `collect`s one and
indexes into it — on a path that runs per fragment during a push and over `MAX_FRAGMENTS` addresses
per client query. A caller asking about a single address — the majority — uses a one-element array
on the stack and allocates nothing at all. A batching caller still allocates a buffer, but owns it:
where it previously took whatever the store returned, it may now size one to its chunk and reuse it
across chunks. The two in the tree, cache fill and push, allocate per batch as they did before, so
what the slice buys them is the option rather than the saving. The call does not become
allocation-free either way, since `ImmutableStore` is `dyn` and `#[async_trait]` boxes each call's
future regardless.

Single-address callers — the majority — get a helper that cannot diverge, written as a free function
rather than a trait method so no store can override it:

```rust
pub async fn query_one(
    store: &Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
) -> Result<StoreMatchResult, StoreError> {
    let mut result = [StoreMatchResult::default()];
    store.clone().query(partition, &[address], &mut result).await?;
    Ok(result[0])
}
```

Reads are gated by `read_scope`, existence by `query_scope`, which reaches further. They differ
because a read hands over bytes and no protocol carries the level alongside them: whatever a store
across a trust boundary serves is read as an association of the caller's own, so such a store serves
the exact association and nothing else. Existence hands over a level, and a partition match is an
instruction to `copy` rather than a promise to serve, so a store may report one where it would
refuse to read. A single-tenant store has no boundary to cross and both scopes fall to `MatchHash`.

Describing a payload is strictly less than handing it over, so a store describes anything it would
serve. Resolving the level *before* applying either scope matters: a lookup caps at the level it is
asked for, so searching at the scope would report every full match as a partition match and lose the
one thing the level is for.

The existence answer carries no fragment, because nothing reads one. Every production consumer of
`query`'s fragment reads exactly `PayloadStoredLocal` and `PayloadStoredDurable` — `stored_flags`
and `follower_future` in the write path, and one helper in `lore-revision`. Those two bits become
two fields, and the fragment stops being a vehicle for smuggling them.

### The contract

Written into the trait, because the failure modes are asymmetric:

1. **Never over-report.** A reported level must hold: the association it names exists. It says
   nothing about whether the payload can be served from here — that is `stored_local` and
   `stored_durable`. A store may hold the representation and not the bytes, and reports a full match
   when it does; the local store does exactly this, and a `get` that fails against a described
   fragment is how the fragment engine knows to fetch it from upstream.
2. **May under-report.** A store may answer with a weaker level than the truth when establishing the
   stronger one costs more than it is worth. A caller must read a weak level as "no shortcut
   available", never as proof of absence.
3. **Obliterated never matches.** At every level, in every store, through `query`, `get_metadata`
   and `get` alike.
4. **Reads do not under-serve, and agree with each other.** A store serves everything within its
   `read_scope`, and `get_metadata` reaches exactly as far as `get` — one that hands over bytes it
   will not describe is answering one question two ways again. `query` reaches further, to
   `query_scope`, because what it reports is a level to act on with another operation rather than
   bytes owed here.
5. **A match names where it was found, and prefers where it was asked.** Every match reports the
   partition the content is in. Where the partition asked about holds the hash, that is the one
   reported; another may be named only when it does not. A store that searched in some other order
   could satisfy every clause above and still hand back a foreign partition while a usable local one
   sat beside it, turning a free copy into one needing a token.

Clause 2 is what makes this safe to land incrementally, and it is safe for callers because
under-reporting only ever costs work: a push re-uploads a payload it could have referenced, a write
re-stores content already stored. Nothing in the tree concludes "absent, therefore I may proceed"
from a weak answer.

The contract binds protocols as well as stores. A handler that answers `query` on behalf of a store,
and the client store that decodes that answer, are together an implementation of this trait spanning
a wire, and the clauses apply to the pair end to end. A protocol may weaken an answer — clause 2
permits it, and the trust-boundary rule below requires it — but it may not strengthen one, report a
level the origin store did not establish, or resurrect obliterated content.

The battery runs against a store reached over a protocol as well as one called in process; its
capabilities carry a flag for answers crossing a trust boundary, and it holds each store to the
scopes it declares rather than to a fixed expectation — a store reading only exact associations must
refuse a sibling context, one reading wider must serve and describe it. The client store runs it
against a server configured as a real one is.

Every store that runs it passes every clause, and none declares a violation. The mechanism stays
because it is what made the defects visible: a store registers the checks it fails, the battery then
*requires* those to fail, and a fix that is not delisted breaks the build as loudly as a regression.
An empty list is the state to hold, not the absence of one.

Six of the seven implementations run it — the local, durable and composite stores, the client store
over a wire, and both replication stores. The gRPC replica is the exception: it answers no reads at
all, so it fails the first check for behaving as designed, and covering it needs the battery to gain
a way to say that a store does not answer and to assert the refusal is uniform.

### Why the levels are the levels

Each level names an action the caller can take, which is what makes it worth reporting:

| level | what the caller may do |
|---|---|
| `MatchFull` | nothing — the association already exists |
| `MatchPartition` | duplicate the association with a **copy**, transferring no payload |
| `MatchHash` | reuse the local payload without recompressing |
| `MatchNone` | store the payload |

`MatchPartition` earns its place on the strength of the copy path: the payload is already in the
partition under another context, so the caller needs an association, not a transfer. The operation
exists on the wire (`session.copy`, whose handler is documented as in-partition or cross-partition
duplication without payload transfer) and the status byte that carries the level already reaches
clients. Any local store establishes the level for free, in one bucket pass, so every cache in the
stack — on the client and on the server — is a place the answer can come from.

This is the step the present work exists to enable, and it is a small change once the answer is
trustworthy; this proposal's obligation is to stop discarding the information it needs and to make
what survives mean the same thing everywhere. The AWS store is the exception: it declines to
establish `MatchPartition` on the existence path, at a bounded cost set out under **AWS** below.

`MatchHash` is a level the AWS store should not answer at all. A connection is authorized for a
finite set of partitions, so a hash found outside that set names a source the caller cannot use in a
copy, however cheaply it was found — and naming it would disclose another tenant's holdings, which
is the collapse rule's subject. The store isolates partitions, so its `query_scope` already forbids
it; the point here is that nothing is lost, because the answer would have been unusable even if it
were free. Finding one is not free either: the associations table is keyed by hash, so a hash-wide
search is a `Query` per address rather than part of the batch. The level belongs to stores holding
several partitions the caller can already reach.

### What each store does

**Local.** `find()` already returns the best match in one bucket pass, so it loses its
`match_request` parameter and resolves at full strength for every caller. The bucket search beneath
it keeps the parameter, because `verify_fragment` still probes at a level deliberately.
`stored_local` comes from `pack_file != 0`, `stored_durable` from the fragment's own durable flag or
a store configured to imply it. All three operations are `find` at full strength, gated afterwards —
`get_metadata` and `get` on `read_scope`, `query` on `query_scope`. The same store is both kinds
depending on where it runs: the server sets `isolate_partitions`, a client leaves it off.

**AWS.** Existence becomes a single `BatchGetItem` on the associations table: `MatchFull` where an
association is present, `MatchNone` where it is not. `exist_batch_exact` and `exist_batch_inexact`
are deleted with the operations they served; the per-address helpers stay, because `get` and
`get_metadata` still resolve one address at a time and can afford to.

One read suffices because of two orderings, which this proposal makes load-bearing rather than
incidental. A put writes the association **last**, after the payload is in S3 and the state is
published, so a visible association already means the content is there. An obliteration deletes the
association **first**, before it marks the hash, because the reference is the obligation and the
mark is only bookkeeping for the reclamation that follows; it deletes again once the mark is up, so
a writer that raced the first delete does not outlive the obliteration. A reader inside that window
is indistinguishable from one that arrived a moment earlier, which obliteration does not order
against either.

While the legacy fragment metadata table is still configured the invariant does not hold for
everything in the store: a fragment written before the state table existed has no state row, and its
lifecycle lives in the flags of its metadata row instead. So long as that table is configured, every
association is confirmed against the state table, and any hash with no state row is looked up in the
metadata table. That is three batched reads rather than one, none of them per address: the first two
need nothing from each other and go out together, and only the legacy lookup waits, because what it
asks for is whatever those two left unresolved. Each fans out to one DynamoDB request per hundred
addresses, so the cost lands on peak concurrency rather than on serial round trips, of which there
are two. When the table is retired the confirmation goes with it and existence is the single batched
read.

The fallback is not only about obliteration. Without it an address stored in the legacy era resolves
to absence, and a push would tell a client to upload content the store already holds — content
uploaded long enough ago that the client has no copy left to send.

An unassociated hash reports absence, which is not a cost but the requirement: this store isolates
partitions, so the existence of content it holds for someone else is not the caller's to learn.

What it does give up is `MatchPartition` on this path — a hash the caller's own partition holds
under another context, which it is entitled to know about and could copy from. Establishing that
needs a DynamoDB `Query` per address rather than one batched read, on a path that runs once per
fragment during ingress, and clause 2 permits the weaker answer. The loss is bounded because this
store is rarely asked alone: it sits under a composite, behind a local cache on the server and
another in front of the session on the client, and any of those that has seen the content in this
partition establishes the level in one bucket pass. What remains exposed is a cold cache, or a
deployment with the AWS store alone; whether to spend the extra query there can be revisited once a
caller exists that acts on the answer.

Reads cost one lookup. Isolating partitions puts `read_scope` at the exact association, so a miss is
the answer rather than the start of a wider search, and the descent through weaker levels goes with
the `Query` helpers that served it — the only `Query` this store still issues counts the references
keeping a payload alive. Restoring a partition match here means restoring that helper, not
re-enabling a branch.

**Composite.** Existence becomes one fan-out over a batch, where it was three over single addresses.
The stopping rule stops being a comparison against a requested level and becomes: keep fanning out
while any address is below `MatchFull`; then per address take the highest level any store reported
and **or** `stored_durable` across responders. `stored_local` is not merged: it stays as the local
store left it, because a replica reporting the payload on *its* disk says nothing about ours. The partition travels with the level that won rather
than being merged on its own — it names where *that* store found the content, and pairing one
store's partition with another's level would point a copy at somewhere the content was never seen.

Taking the maximum, rather than the winning store's answer whole, is what lets a composite answer
better than the store underneath it: a cache that knows the hash sits in this partition supplies
`MatchPartition` even when the durable store below reports less. Under the current arrangement the
winner's fragment is taken entire, so one store's report about local residency can be served as if
it were another's, and a level one store established can be lost because a different store answered
first.

`get_metadata` keeps a fan-out of its own, and should: a representation is worth asking replicas
for, so an edge region can answer without a cross-region trip to the durable store. What changes is
that a level below `MatchFull` is an answer rather than a miss, and that a representation which had
to come from elsewhere is cached locally without its payload.

Each child fills a scratch buffer which the composite merges into the caller's slice, which halves
what a child costs: the batched fan-out it replaces collected the pending addresses into a `Vec` of
its own *and* returned its answers in another, where now the addresses are shared behind one
reference and only the answers are allocated. The buffer itself stays per child, since they are
spawned and a shared one sliced between them would have to outlive tasks that own their futures.
Above them the caller's slice means the call allocates nothing to return.

**Replica and ReplicatedStore.** One method, one request, one set of labels. `get_metadata` stops
being `query(MatchFull)` and becomes a request of its own, so it answers with a representation
rather than with durability.

**Remote.** The batch call decodes one status per address. The payload download disappears with the
old `query`. `get_metadata` already uses the right wire operation.

**gRPC replica.** Three unsupported stubs instead of five, and still no reads of any kind.

**The eleven test doubles.** Two canned methods instead of four.

### Naming the source

A copy names a partition to read from, so an answer reporting only a level leaves a caller unable to
act on it. Every match therefore carries the partition the content was found in.

The level bounds who can name what, without any new configuration. `MatchFull` and `MatchPartition`
name the partition the caller asked about, which it already holds, so no information is added.
`MatchHash` is the only level that names somewhere else, and a store that isolates partitions cannot
report it at all: its `query_scope` stops at the partition. The only thing that can name a foreign
partition is therefore a store that may read across them: a client's local cache, answering in
process.

That is also why neither wire grows a field. `MatchHash` collapses to absence at a trust boundary,
and both protocols are partition-bounded, so every answer crossing either of them was found in the
partition already named in the request.

The source is a hint, not an authorization. A caller acts on it by naming that partition in a copy,
and a server that authenticates verifies access then — so a store naming a partition the caller
cannot use costs a refused copy, not a breach. A server running without authentication has no
identities to check and grants every partition to every caller, which is what running without
authentication means; the hint gives such a caller nothing it could not already ask for. Where a
hash sits in several partitions the store names one of them; a caller that cannot use it uploads,
which is what it would have done anyway.

### Existence over the wire

The mapping is unchanged; what it enforces becomes explicit, and stays in exactly one place:

- `MatchFull` → status 0
- `MatchPartition` → status 1
- `MatchHash`, `MatchNone` → status 3

`MatchHash` never crosses a trust boundary, because a hash the caller's partition does not hold is
another tenant's content and its existence is not the caller's to learn. `MatchPartition` does
cross, because the caller already holds the partition it refers to — and because it is the level
that tells a client a copy will do instead of a payload transfer, it is the one level whose crossing
is worth the byte.

Two corrections, neither a format change: `ExistHashMatch` is renamed to say partition, and the
remote store decodes status 1 as `MatchPartition` rather than demoting it to `MatchHash`. The one
consumer that cares tests `>= MatchHash`, which a partition match satisfies.

On the replication wire, the metadata operation has its own command and its own request carrying the
single address it describes, rather than borrowing the existence payload and rejecting anything but
one address. The single-address special case in the existence handler disappears with the divergence
it was compensating for. The commands the borrowed shape used are left unassigned rather than
reused, so a peer that still sends one is rejected instead of misread, and the level byte on both
remaining requests becomes reserved rather than removed, so no layout moves.

### Reads and partition scope

`get` loses its parameter. `LOCAL_ISOLATION`, `ReadOptions::isolate`, `with_isolation()` and
`no_isolation()` are deleted, and the flag becomes a store setting beside `implicit_durable_stored`.
The read path's full-then-fallback pair collapses to a single call, and the two server handlers that
re-assert isolation stop doing so — a server's store isolates already, so they were asking for what
they had.

Where a handler needs "this address belongs to this repository" as an *authorization* check rather
than a read-scope hint, that is `query` requiring `MatchFull` before serving: one cheap call that
states the check instead of hiding it in a read flag.

## Compatibility

The trait is internal, so most of the churn is confined to the tree.

- **Wire format** — No layout change on either protocol. The client protocol's `QueryStatus` keeps
  its discriminants — `0`, `1`, `3`, with `2` still unused — and only the name of status 1 and its
  decoding change. On the replication protocol the byte that carried the requested match level
  becomes reserved rather than removed, written as `StoreMatch::MatchFull as u8` and ignored on
  read, so both requests and responses stay byte-identical.
- **Client/server protocols** — No new or changed client RPC, and `lore-proto` has no diff, so the
  gRPC storage service is untouched. A new client decodes status 1 as `MatchPartition` where it
  previously decoded `MatchHash`; the change is upward, and the one consumer that cares tests for at
  least `MatchHash`, which a partition match satisfies. On the replication protocol the
  single-address `Query` (commands 4 and 7) is removed, and the `get_metadata` commands 9 and 10
  already exist on `main`, so a new peer sends nothing an old server does not know. In the other
  direction an upgraded server rejects an old peer's `Query` as an unknown command; that degrades
  rather than breaks, because the old peer's composite treats a replica error as no contribution and
  answers from its remaining stores. An old peer's requested level is now ignored and it receives
  the level actually found, which is never weaker than what it asked for, so its `>= requested`
  comparisons still hold.
- **On-disk format** — Unchanged. No fragment flag, index, pack or bucket-entry change: `find()`
  loses a parameter, not a format. The DynamoDB schema is unchanged (no new table, key or
  attribute), and only the order in which `put` and `obliterate` write existing items differs. An
  upgraded and a downgraded process read the same repositories and the same tables.
- **CLI and public API** — No surface change: `lore-capi/lore.h` has no diff on this branch, and no
  `lore` subcommand, exit code or output format changes. `LOCAL_ISOLATION` was `pub` from
  `lore-storage` but set only inside the server, so deleting it moves nothing a script or
  integration could reach. One printed *value* does change, following clause 3: `lore file dump` of
  an obliterated address reports no match and zeroed sizes where it used to print the tombstone's
  flags. Describing obliterated content is what clause 3 forbids, so the dump reporting absence is
  the point rather than a casualty; a script keying on the obliterated flag in that output has to
  key on the absent match instead.

**One deliberate behaviour change.** An obliterated hash stops reporting as existing on the AWS
`exist` path — nothing regresses, since the put path already consults `query`, which already
refuses. Read reach is otherwise as it was: a server has always answered reads from the exact
association, a client has always widened to the hash, and expressing that as two store-side scopes
rather than a per-call level and a process flag leaves both where they were.

The C API keeps its shape and answers from the local store more often. `lore_storage_get_metadata`
routed any non-full local match to the remote, which was necessary while a partial match meant the
representation could not be trusted; it now takes any match the store made. This operation answers
what a payload *is*, and a weaker level names the same bytes under the same hash, so the description
is the same one the round trip would have fetched. What the level would have distinguished — whose
association it is — is not what this call asks about.

## Non-Functional Considerations

- **Concurrency** — Unchanged in shape: the composite keeps its delayed durable spawn, its
  cancellation token and its replica fan-out, now run once over a batch rather than three times over
  single addresses. Each child writes into its own scratch slice and the parent merges, so no result
  buffer is shared between tasks. What becomes load-bearing under concurrency is the AWS write
  order: a put publishes the association last, and an obliteration deletes it first, marks, then
  deletes again, so a racing reader sees the state before or the state after, and a writer that
  raced the first delete does not outlive the obliteration.
- **Memory** — Bounded, and lower where it counts. Existence writes into caller-owned slices, so a
  single-address caller — the majority, including the per-fragment ingress path — allocates nothing
  at all where it previously took a `Vec` from the store. A batching caller still allocates one, but
  now owns it and may reuse it across chunks; the two in the tree have not been changed to, so for
  them the slice buys the option rather than the saving. What remains either way is the boxed future
  `#[async_trait]` produces for a `dyn` trait, which this proposal does not address. The remote store stops
  downloading a whole payload to answer a metadata question, which removes the one place on this
  path that buffered data proportional to content size. On the durable store a put no longer copies
  the payload to hand it to S3, since the buffer it already holds is refcounted and the SDK takes
  one directly, and a read sizes its buffer to the object rather than to the largest object a
  fragment may be — so the transient held per concurrent read is the fragment, not the bound.
- **Statelessness** — Strictly less process state. `LOCAL_ISOLATION`, a process-wide `AtomicBool`
  consulted on every read, is deleted, and read scope becomes a field on the store that owns the
  decision. Nothing new survives across operations.
- **Determinism** — Same inputs, same outputs, with one qualification the contract makes explicit: a
  reported level is a lower bound rather than a function of the address alone, so two runs against
  differently warmed caches may report `MatchPartition` and `MatchFull` for the same address.
  Callers may depend on the floor, which is never over-reported, and not on the exact level — which
  is what makes clause 2 safe to rely on. Content, addressing and history are unaffected.
- **Latency and cost** — Cost moves down. The AWS existence path keeps its single `BatchGetItem` and
  drops the state read it would otherwise need for clause 3, paying in obliteration instead: one
  extra association delete on an operation that is rare, to save a read on one that runs per
  fragment during ingress. Its reads lose a round trip too, since a store answering only exact
  associations never runs the `Query` that would reach a sibling context. The read path stops making
  two calls where one will do, and the composite stops running three existence fan-outs where one
  suffices.

The largest effect is on the code rather than the wire. Four existence-shaped methods across
eighteen implementations become one, beside the metadata read that was already there; the
composite's five fan-outs become three — one for existence, one for representations, one for
payloads; and the AWS store loses the two batch resolution helpers along with the levels no caller
can ask for.

## Migration Plan

`N/A — no breaking changes, no migration required.` Both wire layouts are preserved, and the removed
replication commands degrade to a weaker answer rather than to an error a caller cannot absorb, so
there is no flag day and no ordering requirement between server and client rollouts. The one
sequenced item predates this proposal:
[LEP 2026-08-03](2026-08-03-fragment-metadata-on-the-s3-object.md) governs retiring the legacy
fragment metadata table, which this design reads only while it remains configured.

## Security Considerations

The change makes an existing leak boundary explicit rather than moving it. Today the rule that a
hash-only match must not cross a trust boundary lives in one `match` arm in one request handler, is
not stated in the trait, and is not tested. Under this proposal it is a clause in the contract, a
case in the conformance battery, and a single mapping every path funnels through.

Clause 3 is likewise a security property being made total: obliteration is a compliance obligation,
and the current arrangement discharges it on the paths that consult state and not on the paths that
do not.

Reordering obliteration to delete the reference before marking the hash puts the obligation first,
and moves the failure mode to the safe side. An obliteration that dies between the two leaves the
reference gone and the payload unreclaimed, with no mark to find it by — a leak rather than a
surviving reference. It is recoverable, because the hash is still `Stored` and a re-run proceeds
normally, so until a sweeper exists an obliteration must be driven to completion by whatever
requested it. Under the previous order the same crash left the mark set and the reference intact,
where a re-run reports success without deleting anything.

Clearing up after an obliteration that never finished is separate work: a background task that finds
hashes left marked and either completes the reclamation or releases the mark. Two things constrain
it. The mark has no lease or expiry, so a stranded one is indistinguishable from an obliteration
still in progress by inspection alone. And it cannot simply call `obliterate` again — that returns
success immediately for a hash already marked, which is what makes a stranded mark permanent today:
every write of that hash is refused, in every partition, because the state is keyed by hash alone.

Relaxing reads to allow cross-context within a partition widens what a read can reach. It is bounded
by the partition, which is the unit access is granted on, and it matches what a caller could already
obtain by asking for the address it already holds under its own context.

## Privacy Considerations

The partition on a match is the only new data any caller sees, and the level bounds it. `MatchFull`
and `MatchPartition` name the partition the caller asked about and already holds. `MatchHash` is the
only level that can name somewhere else, a store that isolates partitions cannot report it, and it
collapses to absence at every trust boundary. No answer crossing either protocol therefore names a
partition the request did not already name, and the only place a foreign partition becomes visible
is a client's own cache describing content that client already pulled.

Nothing new reaches logs or telemetry. The repository already appears as a tracing field on the
replication handlers, and this change adds no field and no label; the replica stores lose a set of
labels rather than gaining one.

The change improves deletion rather than affecting it. Obliteration is how a partition's content is
removed on request, and clause 3 makes obliterated content unmatchable through every path in every
store, where today it is refused only where state happens to be consulted.

## Risks and Assumptions

**Assumptions**

- **Assumption:** No caller reads a weak answer as proof of absence; clause 2 rests on this, and
  every present caller either redoes the work or falls back to a stronger path — *invalidated if:* a
  caller is added that skips a write, a repair or a durability guarantee on the strength of a
  non-full match.
- **Assumption:** The AWS write orders hold — a put publishes the association last, an obliteration
  deletes it first — so a visible association implies retrievable content and existence needs one
  read — *invalidated if:* a path writes an association before the payload and state are in place,
  or marks a hash before deleting its references.
- **Assumption:** The AWS store sits under a composite with at least one local store in front of it
  on the ingress path, which is what recovers the `MatchPartition` it declines to establish —
  *invalidated if:* a deployment puts the durable store alone on that path and expects the copy
  optimization to fire.
- **Assumption:** A configured legacy fragment metadata table is a faithful signal that legacy-era
  fragments may still be referenced — *invalidated if:* the configuration is removed while such
  fragments remain, at which point they resolve to absence and a push asks a client to re-upload
  content it no longer holds.

**Risks**

- **Risk:** An obliteration dies after deleting the association and before the mark lands, leaving
  the payload unreclaimed with no mark to find it by — *mitigation:* the hash is still `Stored`, so
  a re-run proceeds normally, and until a sweeper exists the requester drives it to completion.
  Accepted as the safe side of the trade: the previous order left the reference intact instead.
- **Risk:** A store over-reports, and a caller skips an upload or names a partition that cannot
  serve the copy — *mitigation:* clause 1 is the first case in the battery, every implementation
  runs it, and the client store runs it over a wire; a copy naming an unusable partition is refused
  by the server, costing a retry rather than a breach. A caller needing the bytes rather than the
  level reads `stored_local` and `stored_durable`, which the write path already gates on, so a
  representation held without its payload cannot be mistaken for one that can be served here.
- **Risk:** A store resolves a tombstone through a path that does not consult flags. `lookup`
  searches addresses alone, so an obliterated entry still resolves at the level its address matches,
  and every operation answering a caller has to refuse it for itself — which is how `get_metadata`
  came to describe obliterated content while `query` beside it reported nothing — *mitigation:* the
  battery asserts absence through all three operations rather than through the one under change, so
  a path that forgets the check fails rather than passing on its neighbour's behalf.
- **Risk:** While the legacy metadata table is configured, existence costs three batched reads
  rather than one — *mitigation:* all three are batched and none is per address, and the extra two
  retire with the table.
- **Risk:** Clause 3 does not hold for legacy-era content in the AWS store. `obliterate` returns
  early when a hash has no state row, reporting success without deleting the association, while the
  legacy fallback added here goes on resolving that association to a full match — so obliterating a
  fragment written before the state table existed does nothing and says it worked. The early return
  predates this proposal; what is new is the fallback that keeps such content matchable —
  *mitigation:* accepted as a calculated risk, because it reaches only deployments still holding
  content written before the state table existed. The recommended path for those is migrating to the
  new representation under [LEP 2026-08-03](2026-08-03-fragment-metadata-on-the-s3-object.md), which
  ends the exposure by ending the legacy era rather than by teaching this store a second lifecycle
  to obliterate through. Until a deployment has migrated, an obliteration covering legacy-era content
  has to be confirmed rather than assumed.

## Drawbacks

- A cold-cache ingress path against the AWS store alone uploads payloads it could have copied,
  because that store declines `MatchPartition` on the existence path.
- Obliteration costs an extra association delete and an extra round trip.
- The battery's declared-violation list means a fix that is not delisted fails the build. No store
  declares one today, so the cost is latent until the next one is.
- `query` puts a length precondition on its callers that the old `Vec` return did not, held by a
  `debug_assert_eq!`; a release build zips the two slices and answers for the shorter one.

## Alternatives Considered

### Unify the four methods but keep `StoreMatch` as a request

Collapse `exist`, `exist_batch` and `query` into one batched method that still takes a requested
level, leaving callers to ask for what they need.

*Rejected because:* a lookup caps at the level it is asked for, which is precisely how a partition
match reaches the writer as a miss today. It also leaves read scope a per-call value, so
`LOCAL_ISOLATION` and its duplicate policy survive, and it keeps the level a thing eighteen
implementations each interpret.

### Renumber `StoreMatch` to match the wire status codes

Give the enum the wire's values, with not-found at 0, so one set of numbers spans store and
protocol.

*Rejected because:* both orderings are already public, and in opposite directions.
`lore_repository_store_immutable_query_event_data_t.status` is documented in `lore.h` value by value
(0 exact, 1 hash in this repository, 2 hash in another, 3 absent), and every one of the four is
reachable, since the local arm maps `MatchHash` to 2 for an in-process store that reads across
partitions. `lore_file_dump_event_data_t.match_made` carries the `StoreMatch` discriminant verbatim,
which runs the other way with `MatchNone` at 0. Both structs are `Serialize`, so JSON event
consumers read the same numbers. The `Ord` on the enum is load-bearing besides: the `read_scope`
gate, the composite's `max` merge and the battery's bounds are all comparisons, and renumbering
inverts them. Keeping both orders and translating at the two event boundaries is cheaper than
changing a published surface.

### Re-key the AWS associations table on hash and partition

Make the partition part of the primary key so a partition match falls out of the same batched read
that establishes a full match.

*Rejected because:* it is a table migration to buy a level the composite already recovers in the
usual deployment, and obliteration and reclamation key on the hash alone, so the hash-wide access
pattern does not go away.

### Confirm every association against the state table

Keep the second batched read on the existence path permanently, rather than making the put and
obliterate orderings load-bearing.

*Rejected because:* it spends a read per fragment on ingress to avoid one extra delete on
obliteration, which is rare. The orderings it replaces are ones the store wants for its own
correctness in any case.
