---
lep: 2026-11-06-peer-read-fan-out
title: Home nodes for fragment reads in a Lore Server cluster
authors:
  - Raghav Narula
status: Draft
created: 2026-11-06
updated: 2026-11-09
discussion: <link to the review venue>
---
<!-- nox:artifact type=rfc feature=peer-read-fan-out phase=define -->
<!-- NEXT: /nox:design after engineer approval (optional: /nox:submit-docs for standalone team review) -->
<!-- REQUIRES: Engineer approval in-session; RFC committed to the repo -->

# Home nodes for fragment reads in a Lore Server cluster

## Summary

A Lore Server node that misses a fragment in its local cache asks every same-region peer for it at
once, and every peer that holds the fragment sends it, so peer traffic grows with the number of nodes.
This proposal gives every fragment a *home* node. The last bytes of the fragment's hash select one of
a fixed number of *slots*, and every node derives the same slot-to-node assignment from the cluster's
member list by rendezvous hashing. A node that misses a fragment asks only its home. The home answers
from its cache, or asks the one member ranked next for that slot, or reads the durable store, and
caches what it finds. A write to the fragment's home is no longer skipped while a node refreshes its peer
list. No node gains a special role.

## Motivation

Lore Server composes a local cache, peers and a durable store behind one immutable store
(`lore-revision/src/store/composite.rs`). On a local miss, `get_from_remotes` sends a request to every
read replica in parallel. Lore Server asks the durable store after an optional delay
(`durable_store_delay_ms`, zero when unset) or once every peer has missed. `get_metadata` and `query`
fan out the same way. The requests run in a `DetachingJoinSet` (`lore-base/src/runtime.rs`), so they
run to completion after the first reply, and every peer that holds the fragment sends it.

Measurements from a production cluster, comparing three nodes (2026-11-05 08:00–17:50 UTC) with four
nodes (2026-11-05 18:00 to 2026-11-06 06:50 UTC). Peer bytes come from the per-connection QUIC byte
gauges Lore Server records for each peer connection (`lore-server/src/quic/client_monitor.rs`); request
counts come from the composite store and replication client metrics.

| | 3 nodes | 4 nodes |
|---|---|---|
| Peer requests per local miss | 2.02 | 2.99 |
| Successful peer replies per local miss | 1.82 | 2.30 |
| Peer replication traffic | 0.28 GB/s | 0.55 GB/s |
| Peer bytes per client byte | 0.20 | 0.36 |
| Client traffic | 1.38 GB/s | 1.55 GB/s |

Peer requests per miss equal the number of peers, and replies per miss rise with every node added.
Peer traffic doubled for a 12% rise in client traffic.

Lore Server now caches a remote read locally whichever store answered it, so a node no longer
re-fetches the same fragment from its peers on every read. That reduces the number of misses, but every
miss that remains still goes to every peer. Adding a node therefore still raises peer traffic on every
node, and a new node, which misses most often, sends each of those misses to all of its peers.

Any fix must keep a property Lore Server has today: it is leaderless. No node has a special role, and a
client can send any request to any node.

## Goals / Non-Goals

N is the number of members in the cluster.

### Goals

1. **No parallel peer fan-out.** Each local miss sends peer requests one at a time; a node sends the next
   only after the previous one missed or failed.
2. **One home read per miss.** A local miss sends at most one home read, to the fragment's home, and none
   when the node is the home.
3. **Even homes.** Each member's share of slots stays close to 1/N. The relative standard deviation of
   one member's share is √((N−1)/16384): 1.35% at four members and 2.34% at ten.
4. **Minimal movement on membership change.** When a member joins, only the slots it gains change home,
   about 1/(N+1) of them; when a member leaves, only its own slots move, about 1/N.
5. **Warm from the previous home.** After a single member joins, a fragment that moved to it is fetched
   from the member that homed it before, not from the durable store.
6. **Writes reach the home.** Where writes to same-region peers are enabled (`enable_same_region_write`,
   on by default), a write is never skipped for the fragment's home because the replica list is being
   refreshed.
7. **Leaderless.** Lore Server keeps no leader, coordinator or node with a special role, as today. Every
   node returns correct results for every request, and no node's answers are better than another's because
   of its role: on a local miss, every node receives the same answer from the home path.

Goals 1 and 2 hold for each address, on nodes that send home reads. A node with `home_reads` off, or one
that cannot send home reads (Members and member ids), keeps today's fan-out.

### Non-Goals

- **Guaranteed delivery of writes.** Writes stay best effort. A home that missed a write finds the
  fragment on its read path.
- **A sharded cache.** Every node continues to cache everything it reads.
- **Draining a node before removal.** A planned removal behaves like a failure.
- **Searching every member before the durable store.** The home asks at most one peer.
- **Per-request cancellation on the wire.** Peer reads share long-lived QUIC streams, and the server runs
  every request it accepts until it finishes or reaches the server's handling time limit
  (`lore-server/src/quic/stream_handler.rs`). With one request at a time per miss, no parallel losing
  requests remain.
- **Cross-region reads.** Peers in another region already receive writes only
  (`lore-server/src/store/replica_factory.rs`). Home routing considers same-region members only.
- **The mutable store.** Home routing applies to immutable store fragments only.

## Proposed Design

### Members and member ids (Goals 3 and 4)

Every same-region node the topology reports, plus the local node, is a *member* with a *member id*. The
topology reports the local node's own member id as well as each peer's. A member id decides which slots a
node homes, so it must follow the node's cache: it stays the same while the machine and its cache can serve
the slots, and changes only when the node is a different machine. A valid member id is:

- **Unique among live members.** Two running nodes never share an id.
- **The same bytes everywhere.** The id a node reports for itself is exactly the id every peer sees for it.
- **Known locally at startup.** A node obtains its own id without asking another node or a service that
  issues ids, which would be a coordinator (Goal 7).
- **Independent of connections.** Rotating connection ids, reconnecting or changing peer connections leaves
  it unchanged.
- **Stable while the cache survives**, and **new when the machine changes**:

| Operation | Member id |
|---|---|
| Process restart or crash, config reload, connection id rotation | Same |
| Deploy of a new version on the same host | Same |
| Health check failure or short network partition | Same |
| Instance stop and start, or cache wiped, on the same host | Same, though the cache is empty |
| Job moved to a different host, instance replaced, node added | New |
| A second Lore Server process on the same host | Different from the first |

A node that keeps its id with an empty cache keeps its slots and reads the durable store for them until it
warms. Changing the id instead moves about twice as many slots for a similar number of durable reads, so an
id changes only with the machine. Reusing a departed node's id for its replacement is tolerated: the
replacement inherits the slots and starts cold. Each topology chooses where its member ids come from, as long
as they have these properties; Appendix A compares the candidate sources.

- The member list is the set the topology reports, not the set of peers a node managed to connect to.
  A home a node cannot reach counts as a failed home.
- A peer the topology reports without a member id is not a member: it keeps receiving writes but homes
  no slots.
- A peer whose member id equals the local node's is the local node, counted once. A topology that lists
  the local node among its peers, as the Consul source does unless `ignore_address` is set
  (`lore-hashicorp/src/consul/service_peer_discovery.rs`), therefore works unchanged.
- A node sends no home reads, and keeps today's fan-out, when its topology reports no member id for the
  local node, or when two different peers share an id. Such a node still answers home reads that other
  nodes send it.
- A topology composed of several sources reports a local member id when the sources that report one
  agree on it. Sources that report different local ids are a misconfiguration and leave home reads off.
- The topology source is the one input every node shares. When it is slow or unavailable, a node keeps
  routing on the last member list it received.

### Slots (Goals 3 and 4)

A fragment's slot is a number taken from the last bytes of its hash, out of a fixed slot count of 16384.
The slot count never changes for a cluster, because changing it would move almost every fragment.

- **Why the last bytes.** The local immutable store groups and buckets fragments by the first two bytes
  of the hash (`lore-storage/src/local/fan_out.rs`), and each node needs an even spread inside its own
  store. Selecting the home by the last bytes keeps each node's local distribution even, provided the
  bytes of a BLAKE3 hash behave as independent (Risks and Assumptions).
- **Why the hash, not the address.** An address is a hash plus a context. Selecting the home from the
  hash alone sends every context of the same content to the same home, so the home can still report that
  it holds the content under another context or partition.
- **Why 16384.** A member's share of slots varies randomly around 1/N, and more slots make the variation
  smaller (Goal 3). Recomputing the assignment costs one hash per slot per member, about 75 ns each: about
  5 ms at four members and 12 ms at ten, timed with a release build on one core of an Apple M2 Max, once
  per membership change.

### Deriving the home (Goals 3, 4 and 5)

Every node computes the owner of each slot from the member list alone; there is no stored table and no
coordinator. For each slot, each member gets a score: a fixed hash of its member id and the slot number.
The member with the highest score is the slot's *home*. This is rendezvous (highest random weight)
hashing applied per slot.

The scoring has the properties the goals need:

- **Agreement.** The hash is fixed, so nodes with the same member list compute the same home for every
  slot without exchanging messages.
- **Even shares.** Each member's score is in effect an independent draw, so each member wins about 1/N of
  the slots.
- **Minimal movement.** A member's score depends only on its own id and the slot. A member that joins
  takes only the slots where its score beats the current home: about 1/(N+1) of them, an equal share from
  each existing member. A member that leaves gives each of its slots to the member ranked second for that
  slot, and no other slot moves.
- **Known previous home.** When one member joins and nothing else changes, the member ranked second for
  each slot it took is the member that homed that slot before. Its score did not change, and it still
  outranks every other existing member.

A node recomputes the assignment whenever its member list changes. It computes the assignment whether or
not it sends home reads itself, so that it can serve them.

### Reads at the requester (Goals 1 and 2)

On a local miss for an address:

- If the node is the address's home, it runs the home read path itself.
- Otherwise it sends one *home read* to the home. If the home fails, answers `SlowDown`, does not answer
  within the home read timeout, is not reachable, or rejects the request as unknown, the node reads the
  durable store itself.

The node caches what it receives, as it does today. A `query` batch splits by home: each part goes to
its own home, and a failed part falls back to the durable store for only its addresses.

Home reads travel over the same-region read peer connections, so a node with read replicas disabled
(`read_replicas_enabled = false`) sends none. With home reads on, the durable store delay
(`durable_store_delay_ms`) does not apply: the requester reads the durable store only after the home
fails.

Peer reads have no per-request timeout on the client today (`lore-server/src/store/replica.rs`). The
server refuses work beyond a per-connection limit, gives up on a request that waits too long for a handling
permit, and stops a request that exceeds its handling time limit, one hour by default, answering `SlowDown`
in each case (`lore-server/src/quic/stream_handler.rs`).
Neither bounds a read closely enough for a fallback, so this proposal adds a client timeout for home
reads.

### The home read path (Goals 1 and 5)

A home that does not hold a requested fragment:

1. asks the *runner-up*: the highest-ranked member for the slot other than itself, with one local-only
   peer read under its own timeout;
2. on a miss or failure, reads the durable store.

The home caches what it finds, then replies. The requester's home read timeout covers the runner-up
timeout plus a durable read.

- **Local-only peer reads never forward.** A peer answers them from its own cache
  (`lore-server/src/quic/replication_store_service/server.rs`).
- **One hop.** A node that receives a home read runs this path even when its own member list makes another
  node the home. A home read is never forwarded, so members that disagree cannot create a loop.
- **Shared lookups.** Concurrent `get` and `get_metadata` home reads for the same partition and address at
  one home share one lookup; a `query` part is looked up on its own, as `query` is at the requester today.
  A home's own client misses go through the requester path, so a home can briefly have both a
  home read and its own miss outstanding for one fragment.
- **Answers.** For `get`, the home's answer is final, including *not found*, because the home consulted the
  durable store last. For `query` and `get_metadata`, which report how strongly a store matches an address,
  the home merges the answers along its path and stops at the first full match or at the durable store's
  answer, the same stopping rule the composite store applies today.

### Membership changes (Goals 4 and 5)

- **A member joins.** It homes about 1/(N+1) of the slots, taken equally from each existing member. Its first
  read of each such fragment goes to the runner-up, the previous home, so it warms from that member rather
  than the durable store. Existing members keep the copies they cached.
- **Several members join at once.** The runner-up of a moved slot can be another new member with an empty
  cache; those reads go to the durable store.
- **A member leaves or fails.** Each of its slots moves to the member ranked second. The new home holds the
  fragment if its own clients read it, or else asks its own runner-up, then the durable store. Until a node's
  member list drops the departed member, home reads to it fail and fall back to the durable store.
- **A member is replaced under the same member id.** It keeps the same slots and starts with an empty cache,
  which it fills through its read path.
- **Members disagree.** Nodes learn of membership changes at different times. During that window two nodes
  can compute different homes for a moved slot. The cost is extra peer requests or durable reads, never a
  wrong result, because the store is content-addressed.

### Writes (Goal 6)

Writes go to the same write replicas as today, best effort. Today a write skips every peer if a topology
refresh holds the replica-list locks at that moment (`lore-revision/src/store/composite.rs`). With home
routing, a refresh in progress never causes a write to the fragment's home to be dropped: the node resolves
the home from its slot assignment and sends the write over whichever connection to the home exists, the one
from before the refresh or the one after it. Other peers keep today's behaviour.

### Leaderless operation (Goal 7)

Home routing gives each node a role per slot, never a role for the cluster:

- **No coordinator.** Every node derives the slot assignment from its own member list. No node assigns slots,
  holds a table others depend on, or must be available for others to route.
- **Every node does every job.** Every node accepts client requests, acts as a requester, and serves every home
  read it receives. Being a home is a property of a slot, and every member homes about 1/N of them.
- **Correct answers from every node.** The store is content-addressed, so a fragment returned by any node is the
  right fragment. A *not found* from any node reflects the durable store.
- **No node answers better because of its role.** On a local miss, every node sends the read to the same home,
  which follows the same path, so every node receives the same answer from it. The home answers without a peer
  hop, so only its latency is better.
- **Losing a node loses no function.** When a node fails, its slots pass to their runner-ups and every remaining
  node keeps answering every request.

The match level that `query` and `get_metadata` report, and that `get` carries, can still differ between nodes,
as it does today. It differs in these cases:

- **A node's own cache.** Every node answers from its local cache before it sends a home read, and the caches
  differ. A node that holds the content under another context reports that weaker match without asking the home.
- **Fallback to the durable store.** A requester whose home read fails, times out or meets `SlowDown` gets the
  durable store's answer, which can be weaker than the home path's.
- **Member lists that differ.** While members disagree on the member list, or permanently if a node counts itself
  a member while its peers omit it, two nodes can route a slot to different homes.
- **Nodes on today's fan-out.** A node with `home_reads` off, without a local member id, or with duplicate peer
  ids merges every peer's answer as today.
- **Timeout settings that differ** between nodes change when each falls back.

In every case the fragment content is the same, and the difference is in the hint that lets a put become a copy.

### Observability

Lore Server records, per local miss and per operation, the number of peer requests sent, payloads received
and the time to answer. At the home, it records the outcome of each home read (local hit, runner-up hit,
durable read, not found) and the bytes it sends after a durable read. At the requester, it records home
failures by kind: error, `SlowDown`, timeout, rejected. Each node also reports the number of slots it homes,
the writes it sent to and skipped for each fragment's home, and whether it sends home reads, with the reason
when it does not: setting off, no local member id, duplicate peer ids, or sources that disagree. These signals
verify Goals 1 to 7 and measure the costs under Non-Functional Considerations.

## Compatibility

- **Wire format** — The replication protocol gains home read variants of get, get metadata and query, under
  new opcode numbers from 22 upward; the current highest is `ImmutableCopy = 21`, and the protocol never
  reuses a retired number (`lore-server/src/quic/replication_store_service.rs`). Their request and response
  layouts match `ImmutableLocalGet`, `ImmutableLocalGetMetadata` and `ImmutableLocalQuery`. Existing requests
  are unchanged.
- **Client/server protocols** — An old peer that receives a home read answers `InvalidCommand`, keeps the
  stream open and logs a warning (`lore-server/src/quic/stream_handler.rs`); the requester treats this as a
  failed home and reads the durable store. An old requester never sends home reads and keeps fanning out; new
  peers answer its local-only requests as before. The client-facing protocol is unchanged.
- **On-disk format** — N/A. Lore Server caches fragments with the same layout and flags as today.
- **CLI and public API** — N/A. No `lore` command, `lore-capi` or binding changes.
- **Cross-node agreement** — Every member must compute the same slot and ranking, so these are fixed:
  - Slot: the last two bytes of the 32-byte BLAKE3 hash, read as a big-endian integer, keeping the low 14 bits.
  - Score: the first 8 bytes, read as a big-endian integer, of the unkeyed BLAKE3 hash of the ASCII string
    `lore-home-slot-v1`, then the member id's UTF-8 bytes, then the slot as a 2-byte big-endian integer.
  - Order: a higher score ranks higher; between equal scores, the lexicographically lower member id ranks higher.
  - Member ids: every topology must report a member's id as the same bytes, whether the member reports itself
    or a peer reports it.

  Changing any of these moves homes across the cluster and needs its own proposal.
- **Topology plugin interface** — The `Topology` trait (`lore-revision/src/cluster/topology.rs`), which plugin
  crates implement through `TopologyPluginFactory` (`lore-server/src/plugins/traits.rs`), gains a method
  reporting the local node's member id, with a default implementation that reports none. `PeerInfo`
  (`lore-revision/src/cluster/peer.rs`) gains an optional member id. `PeerInfo` has public fields and is built
  with struct literals in plugin crates (`lore-hashicorp/src/consul/service_peer_discovery.rs`) and in the
  built-in topologies (`lore-server/src/topology/`), so each must add the field to compile. `PeerInfo` equality
  covers every field, and a topology refresh compares peers by equality, so a peer whose member id changes is
  treated as one peer leaving and another joining, and its connections are rebuilt. A plugin that leaves both
  empty leaves home routing off.
- **Configuration** — Lore Server gains three settings in the composite immutable store section: `home_reads`,
  which enables sending home reads and is off by default; `home_read_timeout_ms`, the requester's timeout; and
  `runner_up_read_timeout_ms`, the home's timeout for its read from the runner-up.

## Non-Functional Considerations

- **Concurrency** — Concurrent misses for one address on a requester share one home read through the composite
  store's existing in-flight sharing for `get` and `get_metadata`; `query` has no such sharing today and gains
  none, so concurrent queries for one address each send a home read. At the home, concurrent home reads share one
  lookup. Members that disagree on the member list cost extra reads, never wrong results.
- **Memory** — Preserved. The slot assignment is a table of 16384 entries per node.
- **Statelessness** — The slot assignment is process state derived from the member list and rebuilt whenever it
  changes; the home's shared lookups last only as long as their requests. Nothing persists.
- **Determinism** — Changed for `query` and `get_metadata`. Today a requester merges the answers that arrive
  before the durable store's answer or the first full match, so a peer's cache can report a partition or hash
  match that the durable store cannot establish. Such a match lets a client's put become a copy instead of an
  upload (`lore-storage/src/write.rs`). With home routing, only the home's path is merged, so a match held only
  by some other member is missed. Because homes are chosen by hash, every context of the same content shares a
  home, which keeps the common case. Today's result already depends on which answers arrive first. On a local
  miss, every node now receives the same answer from the home path; answers from a node's own cache still differ
  between nodes, as today (Leaderless operation). `get` returns the same content.
- **Latency** — When the home lacks a fragment, the read takes up to two peer round trips, requester to home and
  home to runner-up, before the durable read starts, and the payload then travels from the durable store through
  the home to the requester. Today, with no durable delay configured, the durable read starts alongside the peer
  requests. An unresponsive home costs the home read timeout before the durable-store fallback.
- **Peer transfer on cold misses** — Today a fragment no peer holds costs no peer transfer: the requester reads
  the durable store itself. With home routing, a fragment that the requester does not home and the home must read
  from the durable store costs one transfer from the home to the requester. This proposal accepts that cost: it
  replaces a reply from every peer that holds the fragment on each warm miss, and it leaves the fragment cached at
  its home for every other requester. The home's post-durable-read byte counter measures it.
- **Wasted transfer** — A home that answers after the requester timed out still sends its reply.

## Migration Plan

Home routing changes no format, but it changes what nodes send each other, so it rolls out in two phases.
Topology plugins report member ids from the first phase.

The `PeerInfo` change breaks the build of every topology plugin crate that constructs `PeerInfo` (Compatibility).
Such a crate must add the member id field, or leave it empty, before it builds against the new version; a plugin
that leaves it empty keeps working, with home routing off on the nodes that use it.

| Phase | Versions | `home_reads` | Requests sent | Behaviour |
|---|---|---|---|---|
| 1. Deploy | Old and new mixed | Off everywhere | Today's fan-out | New nodes compute slots and can answer home reads; no node sends them |
| 2. Enable | New everywhere | On per node, one canary first | Home reads from enabled nodes, fan-out from the rest | Every node answers home reads |

- **Signal.** The canary's peer requests per miss, home outcomes, durable reads, peer bytes and answer time,
  compared with the other nodes.
- **Rollback.** Turning `home_reads` off restores today's fan-out on that node at its next restart. Downgrading a
  node's binary while others still send home reads is safe: it answers `InvalidCommand` and logs a warning for
  each one, and the senders read the durable store. No state needs recovery.

## Security Considerations

The trust model does not change. ADR-00023 (`docs/developing/decisions/00023-data-verification-and-trust.md`)
treats peers and replicas as trusted sources and does not verify their payloads. Peers already reach each other
over the authenticated replication transport, and a peer can already make another node read the durable store
through the existing `ImmutableGet` request. Home reads carry the same repository and partition scope as
local-only reads. One home read causes at most one further peer read and one durable read. Because a home serves
its cached fragments to every requester, a corrupt fragment cached at a home now reaches more nodes than before;
this proposal accepts that within ADR-00023's trust model.

## Privacy Considerations

No user data becomes newly visible. Requests between peers carry the same addresses and repository ids as today.
Member ids are node identities, not user data: the Consul node id for Consul, a configured value for the fixed
topologies. The replication metrics keep their existing peer labels.

## Risks and Assumptions

**Assumptions**

- **Assumption:** each topology can supply member ids with the properties in Members and member ids — *invalidated
  if:* a topology can identify a node only by values that change on restart or deploy, which would move that node's
  slots every time.
- **Assumption:** the bytes of a BLAKE3 hash behave as independent and uniform — *invalidated if:* a sample of
  production hashes shows slots uneven, or the last bytes correlated with the first.
- **Assumption:** peer reads, not write replication, dominate peer traffic — *invalidated if:* write replication
  grows to a comparable share of peer bytes.
- **Assumption:** the durable store returns a fragment immediately after it is written, so a home's *not found*
  is final — *invalidated if:* a deployment uses a durable store without read-after-write consistency.

**Risks**

- **Risk:** a popular fragment concentrates load on its home's egress — *mitigation:* every requester caches what
  it receives, so repeat reads become local hits; per-node counters show skew.
- **Risk:** a home whose health check flaps leaves and rejoins the member list, moving its slots each time —
  *mitigation:* members that disagree cost extra reads only; damping membership changes stays open (Unresolved
  Questions).
- **Risk:** a slow home adds latency to every miss it homes, because the requester sends no second peer request —
  *mitigation:* the home read timeout bounds the delay, and the existing unhealthy-client check fails fast for a
  disconnected peer.
- **Risk:** cold misses, common for newly written or rarely read data, gain up to two peer round trips before the
  durable read — *mitigation:* the home asks one peer only; the answer-time counters show the cost.

## Drawbacks

- A miss whose home lacks the fragment waits for peer round trips before the durable read starts.
- An unresponsive home costs a timeout that today's parallel fan-out hides.
- Homes are even only statistically. In a simulation that assigned 16384 slots uniformly at random over 4,000
  trials, the median gap between the busiest and the lightest member was about 3.1% of the average share at four
  members and 7.7% at ten. The gap stays fixed until membership changes.

## Alternatives Considered

### Keep the fan-out (status quo)

Keep asking every peer on a miss, now that peer hits are cached.

*Rejected because:* every remaining miss still sends a request to every peer and receives a reply from every peer
that holds the fragment, so peer traffic per miss still grows with the number of nodes, and a new node still pulls
from every peer.

### Search the other members one at a time at the home

After the runner-up, ask each other member in turn before the durable store.

*Rejected because:* a cold miss then waits for up to N−1 peer round trips before the durable read. Since every node
caches what it reads and the home is where requesters go first, a fragment missing at both the home and the
runner-up is less likely to be held by a third member than to be held by none.

### Stored slot table

Keep an explicit slot-to-node table in a shared store such as Consul's key-value store, edited by one leader that
moves the fewest slots needed for an exact split.

*Rejected because:* it introduces a leader, which Goal 7 rules out, along with locking and a rebalance procedure,
for a gain of a few percent in balance; a stalled leader would leave a new node without slots. The data on each
node is a cache, so a derived assignment's statistical balance costs little.

### Home by hash modulo the number of members

Select the home as the last bytes modulo N.

*Rejected because:* a change from N to N+1 members moves about N/(N+1) of fragments, 75% when going from three
nodes to four, which empties every node's useful cache at once.

### Ring with virtual nodes

Place each member at several positions on a hash ring and give each position the range up to the next.

*Rejected because:* the virtual-node positions form a derived table of the same kind as the slot assignment, with
more entries per member for comparable balance, and the previous owner of a range is a neighbour on the ring rather
than a single, directly computed runner-up.

### Rendezvous hashing on the hash, without slots

Score members against each fragment's hash directly.

*Rejected because:* every read then scores every member instead of looking up one table entry, and each node can
report its share only by counting fragments rather than slots.

### Hedged second request

After a delay, also ask a second member.

*Rejected because:* it adds traffic on every slow read, and the timeout with durable-store fallback already bounds the
delay.

## Prior Art

- **Redis Cluster** maps keys to 16384 hash slots and assigns slots to nodes; adding or removing a node moves slots
  (https://redis.io/docs/latest/operate/oss_and_stack/reference/cluster-spec/). This proposal uses the same slot count
  but derives the assignment instead of storing it.
- **Dynamo** found that a fixed number of equal-sized partitions, assigned to nodes separately from the hash, balanced
  and rebalanced better than random ring positions (DeCandia et al., 2007,
  https://www.allthingsdistributed.com/files/amazon-dynamo-sosp2007.pdf).
- **Rendezvous hashing** (Thaler and Ravishankar, 1998; https://en.wikipedia.org/wiki/Rendezvous_hashing) gives each key
  to the highest-scoring member and moves only the keys of a member that joins or leaves.
- **mcrouter's warm-up route** sends a cold cache's misses to a warm cache and fills the cold one on a hit
  (https://github.com/facebook/mcrouter/wiki/Cold-cache-warm-up-setup). Facebook reports that warming a cold cluster
  from a warm one takes hours instead of days (Nishtala et al., NSDI 2013,
  https://www.usenix.org/system/files/conference/nsdi13/nsdi13-final170_update.pdf). Asking the runner-up applies the
  same idea per slot.
- **groupcache** sends a miss to the key's owner peer, which loads the value once for concurrent callers
  (https://github.com/golang/groupcache).

## Unresolved Questions

- Should a node delay recomputing the slot assignment after a membership change, to damp a member whose health check
  flaps?
- Should a node count itself as a member while its own health check fails, given that its peers' topology then omits it?
- What defaults should `home_read_timeout_ms` and `runner_up_read_timeout_ms` take, and should they scale with payload
  size, given that fragment sizes vary widely?
- Which member id source should each built-in topology use (Appendix A)?

## Appendix A: Member id sources

No topology reports a node's own id today, and the peer ids they report differ:

| Topology | Peer id today | Stable | Node knows its own |
|---|---|---|---|
| Consul | The Consul node id (`lore-hashicorp/src/consul/service_peer_discovery.rs`) | Yes, per host; the Consul agent keeps it in its data directory | No |
| Fixed | A label built from locality, address and port (`lore-server/src/topology/fixed.rs`) | Yes, while the configuration is unchanged | No |
| Rotating | A random value on every rotation (`lore-server/src/topology/rotating_id_fixed.rs`) | No, by design | No |

Three candidate sources, measured against the properties in Members and member ids:

| Property | Advertised address and port | Consul node id | Configured id |
|---|---|---|---|
| Unique among live members | Yes; the port separates processes on one host | No for two processes on one host | Only if operators assign it correctly |
| The same bytes everywhere | Yes, if every node uses the service address its peers see | Yes | Yes, if advertised to peers |
| Known locally at startup | Yes; deployments that set `ignore_address` already supply it | Needs a query to the local Consul agent, or injection at deploy time | Yes |
| Independent of connections | Yes | Yes | Yes |
| Same across restart and redeploy on one host | Yes, while the address and port do not change | Yes | Yes |
| New on replacement, move or scale-out | Yes | Yes | Only if operators issue a new one |
| Works in every topology | Yes | Consul only | Yes, with new configuration in each |
| Changes when it should not | An operator changes the address or port | The Consul agent's data directory is lost | Operator error |

- **Advertised address and port** meets every property without new infrastructure. Every node must use the service
  address its peers see; the Consul source today filters on the node address but builds peers from the service
  address, and the two can differ.
- **Consul node id** is already the Consul peer id, but fails uniqueness for two processes on one host, works only with
  Consul, and needs each node to discover its own id.
- **A configured id** meets every property only if operators never duplicate or reuse it by mistake, and needs new
  configuration and a discovery field in every deployment.
